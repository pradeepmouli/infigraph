#!/usr/bin/env bash
# Infigraph PreToolUse enforcement hook — deny-by-default
# Blocks raw search/file tools in Infigraph-indexed projects.
# The search-fallback sentinel (written by a PostToolUse hook when an infigraph
# search returns nothing) lets raw tools through for five minutes.
#
# MCP-liveness gate: the sentinels below are only ever written after a real
# mcp__infigraph__* call, so none can fire while MCP is unreachable. With no
# infigraph-mcp process at all the hook allows everything; `pgrep` can rule a
# server OUT, never confirm this session can reach one. When a process exists
# but the suggested tool is still unavailable, every denial tells the agent to
# tell the user (to reconnect the server) -- the user is the way out, not a
# bypass the agent writes for itself (#168).
input=$(cat)

# R8.3 (#87): hook/binary version coupling. This script embeds the version
# that installed it (substituted at install time); when the infigraph
# binary on PATH has moved past it, warn -- fail OPEN, never block: drift
# becomes visible instead of silently reverting hook fixes on reinstall
# (I-9). Throttled to once an hour via a stamp file so the version probe
# doesn't tax every tool call.
HOOK_SHIPPED_VERSION="__INFIGRAPH_VERSION__"
case "$HOOK_SHIPPED_VERSION" in
  __INFIGRAPH*) : ;; # unsubstituted (repo copy / dev install) -- skip
  *)
    stamp="${TMPDIR:-/tmp}/.infigraph-hook-version-checked"
    now=$(date +%s)
    last=$(cat "$stamp" 2>/dev/null || echo 0)
    if [ $((now - last)) -ge 3600 ]; then
      echo "$now" > "$stamp" 2>/dev/null
      bin_version=$(infigraph --version 2>/dev/null | awk '{print $2}')
      if [ -n "$bin_version" ] && [ "$bin_version" != "$HOOK_SHIPPED_VERSION" ]; then
        echo "[infigraph-enforce] warning: hooks installed by infigraph v$HOOK_SHIPPED_VERSION but v$bin_version is on PATH -- run 'infigraph install' to refresh them" >&2
      fi
    fi
    ;;
esac

if ! pgrep -f "infigraph-mcp" >/dev/null 2>&1; then
  exit 0
fi

tool=$(echo "$input" | jq -r '.tool_name // empty')
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Guard: only enforce in projects with a .infigraph directory
[ -d "$cwd/.infigraph" ] || exit 0

# Check search-fallback sentinel — if infigraph search returned no results recently, allow raw tools
search_sentinel="$cwd/.infigraph/.search-fallback-allowed"
if [ -f "$search_sentinel" ]; then
  now=$(date +%s)
  sentinel_ts=$(cat "$search_sentinel" 2>/dev/null || echo 0)
  if [ $((now - sentinel_ts)) -lt 300 ]; then
    exit 0
  fi
fi

deny() {
  jq -n --arg reason "$1" \
    '{hookSpecificOutput: {hookEventName: "PreToolUse", permissionDecision: "deny", permissionDecisionReason: $reason}}'
  exit 0
}

# Every denial ends with this. An agent-writable bypass used to be offered
# here, and agents used it as a routine workaround rather than a last resort.
routing="Which tool answers which question: the infigraph-tool-routing skill. If that tool is unavailable or errors, tell the user (e.g. to reconnect the infigraph MCP server with /mcp) -- do not work around this hook."
search_hint="Use mcp__infigraph__search: ranked symbols plus every line containing the text; regex=true lists every matching line (e.g. all call sites)."

# Blank out what the shell never runs as a command: quoted strings (which can
# span lines, as in a multi-line commit message) and heredoc bodies. A command
# word inside one -- `gh issue create --title "grep is blocked"`, a commit
# message mentioning rg -- is text, not a search (#168). Ambiguity resolves
# toward allowing: `bash -c 'grep ...'` passes, and a `<<WORD` inside double
# quotes is taken as a heredoc (so `"$(cat <<'EOF' ...)"` works). This routes
# an agent to the right tool; it is not a sandbox.
strip_unexecuted_text() {
  awk -v q="'" -v dq='"' '
    heredoc != "" {
      line = $0
      if (heredoc_tabs) sub(/^\t+/, "", line)
      if (line == heredoc) heredoc = ""
      next
    }
    {
      out = ""; pending = ""; n = length($0)
      for (i = 1; i <= n; i++) {
        c = substr($0, i, 1)
        if (state == "single") { if (c == q) state = ""; continue }
        if (c == "<" && substr($0, i + 1, 1) == "<" && substr($0, i + 2, 1) != "<" && (i == 1 || substr($0, i - 1, 1) != "<")) {
          rest = substr($0, i + 2)
          tabs = sub(/^-/, "", rest)
          sub(/^[ \t]*/, "", rest)
          while (substr(rest, 1, 1) == q || substr(rest, 1, 1) == dq || substr(rest, 1, 1) == "\\") rest = substr(rest, 2)
          if (match(rest, /^[A-Za-z_][A-Za-z0-9_]*/)) { pending = substr(rest, 1, RLENGTH); pending_tabs = tabs }
        }
        if (state == "double") {
          if (c == "\\") i++
          else if (c == dq) state = ""
          continue
        }
        if (c == "\\") { i++; out = out " "; continue }
        if (c == q) { state = "single"; out = out " "; continue }
        if (c == dq) { state = "double"; out = out " "; continue }
        out = out c
      }
      print out
      if (pending != "") { heredoc = pending; heredoc_tabs = pending_tabs }
    }
  '
}

case "$tool" in
  Grep)
    deny "BLOCKED: Grep on indexed code. $search_hint $routing"
    ;;
  Glob)
    deny "BLOCKED: Use mcp__infigraph__list_files instead of Glob. $routing"
    ;;
  Bash)
    cmd=$(echo "$input" | jq -r '.tool_input.command // empty')
    # Only flag grep/rg-family tools when NOT immediately preceded by a pipe --
    # `cmd 2>&1 | grep -iE "error"` filters another command's output (allowed,
    # matches this repo's own CLAUDE.md guidance); a bare/leading grep call is
    # a code search and should go through mcp__infigraph__search instead.
    scannable=$(printf '%s\n' "$cmd" | strip_unexecuted_text)
    cmd_without_piped_grep=$(printf '%s\n' "$scannable" | sed -E 's/\|[[:space:]]*(grep|egrep|fgrep|rg|ripgrep|ag|ack)([[:space:]]|$)[^|]*/|/g')
    if echo "$cmd_without_piped_grep" | grep -qE '(^|\s|/)(grep|egrep|fgrep|rg|ripgrep|ag|ack)(\s|$)'; then
      deny "BLOCKED: grep/rg on indexed code. $search_hint $routing"
    fi
    if printf '%s\n' "$scannable" | grep -qE '(^|\s)find\s.*-name\s'; then
      deny "BLOCKED: Use mcp__infigraph__list_files instead of find -name. $routing"
    fi
    ;;
  Agent)
    agent_type=$(echo "$input" | jq -r '.tool_input.subagent_type // empty')
    case "$agent_type" in
      Explore|Plan|code-reviewer)
        deny "BLOCKED: $agent_type agents lack MCP access; use a general-purpose agent. $routing"
        ;;
    esac
    ;;
  Read)
    file_path=$(echo "$input" | jq -r '.tool_input.file_path // empty')
    # Allow if offset specified (targeted line-number lookup for Edit)
    has_offset=$(echo "$input" | jq -r '.tool_input.offset // empty')
    if [ -n "$has_offset" ] && [ "$has_offset" != "null" ]; then
      exit 0
    fi
    # Allow if file was recently edited (Edit tracker exemption)
    tracker_file="${TMPDIR:-/tmp}/infigraph-edit-tracker/recent_edits.log"
    if [ -f "$tracker_file" ] && grep -qF "$file_path" "$tracker_file" 2>/dev/null; then
      exit 0
    fi
    # Allow if the file isn't inside the current project directory
    case "$file_path" in
      "$cwd"/*) ;;
      *) exit 0 ;;
    esac
    # Allow if the file is in a directory infigraph excludes from indexing
    rel_path="${file_path#"$cwd"/}"
    case "$rel_path" in
      .infigraph/*|*/.infigraph/*|.claude/*|*/.claude/*|node_modules/*|*/node_modules/*|__pycache__/*|*/__pycache__/*|.tox/*|*/.tox/*|.git/*|*/.git/*)
        exit 0 ;;
    esac
    # Allow if git considers the file ignored (approximates .gitignore; .infigraphignore not covered)
    if command -v git >/dev/null 2>&1 && git -C "$cwd" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
      if git -C "$cwd" check-ignore -q "$file_path" 2>/dev/null; then
        exit 0
      fi
    fi
    # Allow if the file type isn't one infigraph indexes
    base=$(basename -- "$file_path")
    case "$base" in
      Dockerfile|Containerfile|Makefile|makefile|GNUmakefile|CMakeLists.txt|BUILD|BUILD.bazel|WORKSPACE) ;;
      *)
        case "$file_path" in
          *.py|*.rs|*.ts|*.tsx|*.js|*.jsx|*.mjs|*.go|*.java|*.c|*.h|*.cpp|*.cc|*.cxx|*.hpp|*.hxx|*.hh|\
          *.rb|*.rake|*.gemspec|*.php|*.swift|*.kt|*.kts|*.cs|*.scala|*.sc|*.lua|*.zig|*.ex|*.exs|*.dart|\
          *.m|*.mm|*.hs|*.lhs|*.pl|*.pm|*.t|*.r|*.R|*.Rmd|*.ml|*.mli|*.sh|*.bash|*.zsh|*.sql|*.jl|*.proto|\
          *.ps1|*.psm1|*.psd1|*.v|*.sv|*.svh|*.vh|*.hcl|*.tf|*.tfvars|*.toml|*.yml|*.yaml|*.erl|*.hrl|\
          *.f90|*.f95|*.f03|*.f08|*.f|*.for|*.nix|*.svelte|*.fs|*.fsi|*.fsx|*.groovy|*.gradle|*.css|\
          *.html|*.htm|*.json|*.xml|*.xsl|*.xsd|*.svg|*.plist|*.graphql|*.gql|*.glsl|*.vert|*.frag|*.geom|\
          *.comp|*.lisp|*.lsp|*.cl|*.asd|*.elm|*.el|*.ini|*.cfg|*.conf|*.bzl|*.star|*.mlx|*.mat|*.md|\
          *.markdown|*.clj|*.cljs|*.cljc|*.edn|*.cu|*.cuh|*.pas|*.pp|*.dpr|*.dpk|*.inc|*.lpr|*.bas|*.cls|\
          *.frm|*.dockerfile|*.mk|*.cmake) ;;
          *) exit 0 ;;
        esac
        ;;
    esac
    # Block — this file is indexable; use infigraph tools instead. If infigraph search returns nothing, sentinel allows retry.
    deny "BLOCKED: Read on indexed code. A symbol's source: mcp__infigraph__get_code_snippet or get_doc_context. What a file defines, constants included: get_symbols_in_file. Text: search. Read only for exact Edit line numbers (pass offset). $routing"
    ;;
  Write|Edit)
    file_path=$(echo "$input" | jq -r '.tool_input.file_path // empty')
    if echo "$file_path" | grep -qE '(test_[^/]+\.[^/]+|[^/]+_test\.[^/]+|[^/]+\.test\.[^/]+|[^/]+_spec\.[^/]+|[^/]+\.spec\.[^/]+|tests/[^/]+\.[^/]+|__tests__/|\.feature$|\.karate$)'; then
      sentinel="$cwd/.infigraph/.test-context-called"
      if [ -f "$sentinel" ]; then
        # Check freshness — allow if sentinel written within last 30 minutes
        now=$(date +%s)
        sentinel_ts=$(cat "$sentinel" 2>/dev/null || echo 0)
        if [ $((now - sentinel_ts)) -lt 1800 ]; then
          exit 0
        fi
      fi
      deny "BLOCKED: Call mcp__infigraph__generate_test_context before writing tests. $routing"
    fi
    ;;
esac

exit 0
