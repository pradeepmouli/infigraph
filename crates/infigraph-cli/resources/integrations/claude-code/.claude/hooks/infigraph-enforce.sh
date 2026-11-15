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

# Whether $1 (an absolute path) lies where infigraph indexes no code: outside
# the project, in a directory infigraph skips, or git-ignored (approximates
# .gitignore; .infigraphignore is not covered). The hook nudges rather than
# gates, so anything it cannot place inside indexed code is allowed (#76).
outside_indexed_code() {
  case "$1" in
    "$cwd"|"$cwd"/*) ;;
    *) return 0 ;;
  esac
  case "${1#"$cwd"/}" in
    .infigraph|.infigraph/*|*/.infigraph/*|.claude|.claude/*|*/.claude/*|node_modules|node_modules/*|*/node_modules/*|__pycache__/*|*/__pycache__/*|.tox/*|*/.tox/*|.git|.git/*|*/.git/*)
      return 0 ;;
  esac
  command -v git >/dev/null 2>&1 &&
    git -C "$cwd" rev-parse --is-inside-work-tree >/dev/null 2>&1 &&
    git -C "$cwd" check-ignore -q "$1" 2>/dev/null
}

# Whether $1 is source code infigraph extracts symbols from, judged by its
# file name: docs, configs, data and markup (.md, .toml, .yaml, .json, .xml,
# .html, .css, ...) are not (#76). The one list, for the Read and the
# Write/Edit checks alike.
is_source_code() {
  case "$(basename -- "$1")" in
    Dockerfile|Containerfile|Makefile|makefile|GNUmakefile|CMakeLists.txt|BUILD|BUILD.bazel|WORKSPACE) return 0 ;;
  esac
  case "$1" in
    *.py|*.rs|*.ts|*.mts|*.cts|*.tsx|*.js|*.jsx|*.mjs|*.cjs|*.go|*.java|*.c|*.h|*.cpp|*.cc|*.cxx|*.hpp|*.hxx|*.hh|\
    *.rb|*.rake|*.gemspec|*.php|*.swift|*.kt|*.kts|*.cs|*.scala|*.sc|*.lua|*.zig|*.ex|*.exs|*.dart|\
    *.m|*.mm|*.hs|*.lhs|*.pl|*.pm|*.t|*.r|*.R|*.Rmd|*.ml|*.mli|*.sh|*.bash|*.zsh|*.sql|*.jl|*.proto|\
    *.ps1|*.psm1|*.psd1|*.v|*.sv|*.svh|*.vh|*.hcl|*.tf|*.erl|*.hrl|\
    *.f90|*.f95|*.f03|*.f08|*.f|*.for|*.nix|*.svelte|*.fs|*.fsi|*.fsx|*.groovy|*.gradle|\
    *.graphql|*.gql|*.glsl|*.vert|*.frag|*.geom|\
    *.comp|*.lisp|*.lsp|*.cl|*.asd|*.elm|*.el|*.bzl|*.star|*.mlx|\
    *.clj|*.cljs|*.cljc|*.cu|*.cuh|*.pas|*.pp|*.dpr|*.dpk|*.inc|*.lpr|*.bas|*.cls|\
    *.frm|*.dockerfile|*.mk|*.cmake) return 0 ;;
  esac
  return 1
}

# Whether $1 is a test the Write/Edit check holds to generate_test_context: a
# code (or .feature/.karate) file inside the project that a name or a
# directory says is a test. Judged by path COMPONENT, never by a substring of
# the whole path: a Claude memory note called feedback_vitest_json_flag.md, or
# a docs file under contests/, is not a test.
is_test_file() {
  outside_indexed_code "$1" && return 1
  base=$(basename -- "$1")
  case "$base" in
    *.feature|*.karate) return 0 ;;
  esac
  is_source_code "$1" || return 1
  case "$base" in
    test_*.*|*_test.*|*.test.*|*_spec.*|*.spec.*) return 0 ;;
  esac
  case "$(basename -- "$(dirname -- "$1")")" in
    tests) return 0 ;;
  esac
  case "/${1#"$cwd"/}" in
    */__tests__/*) return 0 ;;
  esac
  return 1
}

# What `grep`, `rg` and the Grep tool may search without a block: anything that
# cannot reach source code infigraph indexes. A search is blocked when it names
# a source file or a directory inside the project (a directory may hold code),
# searches the project tree by default (rg, ag, ack, `grep -r`, `git grep`, the
# Grep tool with no path) unrestricted, or when its targets cannot be
# determined (a variable, command substitution, an unterminated quote,
# `xargs grep`): writing it oddly must not be the way round the hook. Docs,
# configs, files outside the project and stdin are not blocked. ripgrep type
# names (not extensions) that name no source code; each, read as an
# extension, is not source by `is_source_code`.
NON_CODE_RG_TYPES="md markdown json toml yaml xml html css txt csv"
way_through="Searching docs, configs or files outside the project is not blocked: name those files, or restrict with --include='*.md' (a directory may hold code, so a bare directory is still blocked)."

# Whether the glob $1 selects only non-code files: exactly `*.<ext>` for an
# extension `is_source_code` does not know.
noncode_glob() {
  g=${1##*/}
  case "$g" in "*."*) ;; *) return 1 ;; esac
  g=${g#\*.}
  case "$g" in ""|*[!A-Za-z0-9]*) return 1 ;; esac
  is_source_code "x.$g" && return 1
  return 0
}

# Whether the search described by the globals reaches source code: OPS (the
# paths it names), STDIN (0/1: reads a pipe or heredoc), TREE (0/1: searches
# the project tree when it names nothing), RESTRICT (0/1: a glob or type holds
# it to non-code), UNSURE (0/1: a target could not be determined).
search_reaches_code() {
  [ "$UNSURE" = 1 ] && return 0
  if [ "${#OPS[@]}" -eq 0 ]; then
    [ "$STDIN" = 1 ] && return 1
    [ "$TREE" = 1 ] || return 1
    [ "$RESTRICT" = 1 ] && return 1
    return 0
  fi
  for p in "${OPS[@]}"; do
    case "$p" in "~"|"~/"*) p="$HOME${p#"~"}" ;; esac
    case "$p" in /*) abs=$p ;; .) abs=$cwd ;; *) abs="$cwd/${p#./}" ;; esac
    abs=${abs%/}
    outside_indexed_code "$abs" && continue
    case "$p" in
      *[\*\?\[]*) noncode_glob "$p" && continue; return 0 ;;
    esac
    if [ -d "$abs" ]; then [ "$RESTRICT" = 1 ] && continue; return 0; fi
    # A path that is not there and has no extension is taken for a directory.
    if [ ! -e "$abs" ]; then
      case "${p##*/}" in *.*) ;; *) [ "$RESTRICT" = 1 ] && continue; return 0 ;; esac
    fi
    is_source_code "$p" && return 0
  done
  return 1
}

# Sets RESTRICT from the globs in $GLOBS and the types in $TYPES (space
# separated): set, and every one non-code.
set_restriction() {
  RESTRICT=0
  [ -n "$GLOBS$TYPES" ] || return 0
  set -f # a glob is a pattern here, not files to expand
  for g in $GLOBS; do noncode_glob "$g" || { set +f; return 0; }; done
  for t in $TYPES; do case " $NON_CODE_RG_TYPES " in *" $t "*) ;; *) set +f; return 0 ;; esac; done
  set +f
  RESTRICT=1
}

# One record per segment of the command on stdin whose command word is a
# grep-family tool (or that wraps one: `xargs grep`): S tool piped heredoc,
# then O path | R redirected-in file | G glob | T type | X recursive | U
# undeterminable, then E. Quote-aware; a `$` outside single quotes, an unquoted
# `{` (brace expansion) or an unterminated quote is U.
grep_records() {
  awk -v q="'" -v dq='"' '
    function wend() { if (inw) { if (rout) rout = 0; else if (rin) { rin = 0; nr++; RW[nr] = w; RD[nr] = wd } else { nw++; W[nw] = w; D[nw] = wd } inw = 0; w = ""; wd = 0 } }
    function fdword() { if (inw && !wd && w ~ /^[0-9]+$/) { inw = 0; w = "" } }
    function segend(p) { wend(); if (nw > 0 || nr > 0) judge(); nw = 0; nr = 0; hd = 0; rin = 0; rout = 0; piped = p }
    function fam(x) { return x == "grep" || x == "egrep" || x == "fgrep" || x == "rg" || x == "ripgrep" || x == "ag" || x == "ack" }
    function judge(   i, a, first, tool, start, haspat, patdone, nxt, isg, out) {
      i = 1
      while (i <= nw && (W[i] ~ /^[A-Za-z_][A-Za-z0-9_]*=/ || W[i] == "do" || W[i] == "then" || W[i] == "else" || W[i] == "elif" || W[i] == "if" || W[i] == "while" || W[i] == "until" || W[i] == "time" || W[i] == "!")) i++
      first = W[i]; sub(/.*\//, "", first)
      if (first == "xargs" || first == "env" || first == "sudo" || first == "nohup" || first == "nice" || first == "exec" || first == "command" || first == "watch") {
        for (a = i + 1; a <= nw; a++) { x = W[a]; sub(/.*\//, "", x); if (fam(x)) { sawgrep = 1; print "S\twrapped\t0\t0"; print "U"; print "E"; return } }
        return
      }
      if (fam(first)) { tool = first; start = i + 1 }
      else if (first == "git" && W[i + 1] == "grep") { tool = "git-grep"; start = i + 2 }
      else return
      sawgrep = 1
      isg = (tool == "grep" || tool == "egrep" || tool == "fgrep")
      for (a = start; a <= nw; a++) if (W[a] == "-e" || W[a] == "-f" || W[a] ~ /^--(regexp|file)=/ || W[a] == "--regexp" || W[a] == "--file") haspat = 1
      print "S\t" tool "\t" piped "\t" hd
      for (a = start; a <= nw; a++) {
        x = W[a]
        if (x == "--") { for (a = a + 1; a <= nw; a++) emit(a); break }
        if (substr(x, 1, 1) == "-" && length(x) > 1) {
          if (x == "-e" || x == "-f" || x == "-m" || x == "-A" || x == "-B" || x == "-C" || x == "-d" || x == "-D" || x == "-j" || x == "--exclude" || x == "--exclude-dir" || x == "-T" || x == "--type-not" || x == "--ignore-file" || x == "--regexp" || x == "--file" || (!isg && x == "-r")) a++
          else if (x == "-g" || x == "--glob" || x == "--include") { a++; out = W[a]; if (D[a]) print "U"; else print "G\t" out }
          else if (x == "-t" || x == "--type") { a++; out = W[a]; if (D[a]) print "U"; else print "T\t" out }
          else if (x ~ /^--(include|glob)=/) { out = x; sub(/^[^=]*=/, "", out); print "G\t" out }
          else if (x ~ /^--type=/) { out = x; sub(/^[^=]*=/, "", out); print "T\t" out }
          else if (isg && (x == "--recursive" || x ~ /^-[A-Za-z]*[rR]/)) print "X"
          continue
        }
        if (!haspat && !patdone) { patdone = 1; continue }
        emit(a)
      }
      for (a = 1; a <= nr; a++) { if (RD[a]) print "U"; else print "R\t" RW[a] }
      print "E"
    }
    function emit(a) { if (D[a]) print "U"; else print "O\t" W[a] }
    heredoc != "" { l = $0; if (hdtabs) sub(/^\t+/, "", l); if (l == heredoc) heredoc = ""; next }
    {
      line = $0; n = length(line)
      for (i = 1; i <= n; i++) {
        c = substr(line, i, 1)
        if (st == "s") { if (c == q) st = ""; else w = w c; continue }
        if (st == "d") {
          if (c == "\\") { i++; w = w substr(line, i, 1); continue }
          if (c == dq) { st = ""; continue }
          if (c == "$" || c == "`") wd = 1
          w = w c; continue
        }
        if (c == q) { st = "s"; inw = 1; continue }
        if (c == dq) { st = "d"; inw = 1; continue }
        if (c == "\\") { i++; w = w substr(line, i, 1); inw = 1; continue }
        if (c == " " || c == "\t") { wend(); continue }
        if (c == "#" && !inw) break
        if (c == "|") { nx = substr(line, i + 1, 1); if (nx == "|") { segend(0); i++ } else { if (nx == "&") i++; segend(1) } continue }
        if (c == ";" || c == "(" || c == ")") { segend(0); continue }
        if (c == "&") { if (substr(line, i + 1, 1) == ">") { wend(); rout = 1; i++ } else segend(0); continue }
        if (c == ">") {
          fdword(); wend(); if (substr(line, i + 1, 1) == ">") i++
          if (substr(line, i + 1, 1) == "&") { i++; while (substr(line, i + 1, 1) ~ /[0-9-]/) i++; continue }
          rout = 1; continue
        }
        if (c == "<") {
          fdword(); wend()
          if (substr(line, i + 1, 1) == "<") {
            hd = 1; i++; if (substr(line, i + 1, 1) == "-") { hdtabs = 1; i++ }
            rest0 = substr(line, i + 1); rest = rest0; sub("^[ \t]*[" q dq "\\\\]*", "", rest)
            if (match(rest, /^[A-Za-z_][A-Za-z0-9_]*/)) {
              pend = substr(rest, 1, RLENGTH)
              i += length(rest0) - length(rest) + RLENGTH
              if (substr(line, i + 1, 1) == q || substr(line, i + 1, 1) == dq) i++
            }
          } else rin = 1
          continue
        }
        if (c == "$" || c == "`" || c == "{") wd = 1
        w = w c; inw = 1
      }
      if (st == "") { segend(0); if (pend != "") { heredoc = pend; pend = "" } } else { w = w " " }
    }
    END {
      if (st != "") {
        wend(); for (a = 1; a <= nw; a++) if (fam(W[a])) sawgrep = 1
        if (sawgrep) { print "S\tunterminated\t0\t0"; print "U"; print "E" }
      }
    }
  '
}

# Whether any segment of the command on stdin searches source code.
command_reaches_code() {
  OPS=(); STDIN=0; TREE=0; UNSURE=0; GLOBS=""; TYPES=""
  while IFS=$'\t' read -r kind a b c; do
    case "$kind" in
      S) OPS=(); UNSURE=0; GLOBS=""; TYPES=""
         STDIN=$b; [ "$c" = 1 ] && STDIN=1
         case "$a" in grep|egrep|fgrep) TREE=0 ;; *) TREE=1 ;; esac ;;
      O|R) OPS+=("$a") ;;
      G) GLOBS="$GLOBS $a" ;;
      T) TYPES="$TYPES $a" ;;
      X) TREE=1 ;;
      U) UNSURE=1 ;;
      E) set_restriction
         if search_reaches_code; then return 0; fi ;;
    esac
  done
  return 1
}

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
    # The Grep tool: path, glob and type say what it reaches (no path: the
    # project tree).
    IFS=$'\037' read -r g_path GLOBS TYPES <<EOF_GREP_FIELDS
$(echo "$input" | jq -r '[.tool_input.path // "", .tool_input.glob // "", .tool_input.type // ""] | join("\u001f")')
EOF_GREP_FIELDS
    OPS=(); [ -n "$g_path" ] && OPS=("$g_path")
    STDIN=0; TREE=1; UNSURE=0; set_restriction
    search_reaches_code || exit 0
    deny "BLOCKED: Grep on indexed code. $search_hint $way_through $routing"
    ;;
  Glob)
    deny "BLOCKED: Use mcp__infigraph__list_files instead of Glob. $routing"
    ;;
  Bash)
    cmd=$(echo "$input" | jq -r '.tool_input.command // empty')
    # Only flag grep/rg-family tools when NOT immediately preceded by a pipe --
    # `cmd 2>&1 | grep -iE "error"` or `cmd |& rg warn` filters another
    # command's output (allowed, matches this repo's own CLAUDE.md guidance);
    # a bare/leading grep call is a code search and should go through
    # mcp__infigraph__search instead. `|&` is bash/zsh shorthand for `2>&1 |`.
    scannable=$(printf '%s\n' "$cmd" | strip_unexecuted_text)
    cmd_without_piped_grep=$(printf '%s\n' "$scannable" | sed -E 's/\|&?[[:space:]]*(grep|egrep|fgrep|rg|ripgrep|ag|ack)([[:space:]]|$)[^|]*/|/g')
    if echo "$cmd_without_piped_grep" | grep -qE '(^|\s|/)(grep|egrep|fgrep|rg|ripgrep|ag|ack)(\s|$)'; then
      # Looks like a search: blocked only if it can reach source code.
      if printf '%s\n' "$cmd" | grep_records | command_reaches_code; then
        deny "BLOCKED: grep/rg on indexed code. $search_hint $way_through $routing"
      fi
    fi
    if printf '%s\n' "$scannable" | grep -qE '(^|\s)find\s.*-name\s'; then
      # The start paths are the words after `find` up to its first option;
      # none means `.`. Allowed only when every one is outside indexed code,
      # such as a build directory (#76).
      find_paths=$(printf '%s\n' "$scannable" | grep -oE '(^|\s)find\s[^|;&]*' | head -1 |
        awk '{for (i = 2; i <= NF; i++) { if ($i ~ /^[-(!]/) exit; print $i }}')
      [ -n "$find_paths" ] || find_paths=.
      while IFS= read -r p; do
        case "$p" in /*) abs=$p ;; .) abs=$cwd ;; *) abs="$cwd/${p#./}" ;; esac
        outside_indexed_code "${abs%/}" ||
          deny "BLOCKED: Use mcp__infigraph__list_files instead of find -name. $routing"
      done <<EOF_FIND_PATHS
$find_paths
EOF_FIND_PATHS
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
    outside_indexed_code "$file_path" && exit 0
    # Allow unless the file is source code infigraph extracts symbols from
    # (`is_source_code`): everything else is read directly, nothing better
    # answers "what does this say".
    is_source_code "$file_path" || exit 0
    # Block — this file is indexable; use infigraph tools instead. If infigraph search returns nothing, sentinel allows retry.
    deny "BLOCKED: Read on indexed code. A symbol's source: mcp__infigraph__get_code_snippet or get_doc_context. What a file defines, constants included: get_symbols_in_file. Text: search. Read only for exact Edit line numbers (pass offset). $routing"
    ;;
  Write|Edit)
    file_path=$(echo "$input" | jq -r '.tool_input.file_path // empty')
    if is_test_file "$file_path"; then
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
