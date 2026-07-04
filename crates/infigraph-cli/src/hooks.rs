use anyhow::Result;
use serde_json::json;

pub(crate) const ENFORCE_HOOK_SCRIPT: &str = r#"#!/usr/bin/env bash
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
  Agent)
    agent_type=$(echo "$input" | jq -r '.tool_input.subagent_type // empty')
    case "$agent_type" in
      Explore)
        echo "WARNING: Do NOT use Explore agent — it lacks MCP access. Use infigraph search/search_code/search_symbols directly, or use general-purpose agent." >&2
        ;;
      Plan)
        echo "WARNING: Do NOT use Plan agent — it lacks MCP access. Use infigraph get_architecture/get_skeleton directly, or use general-purpose agent." >&2
        ;;
      code-reviewer)
        echo "WARNING: Do NOT use code-reviewer agent — it lacks MCP access. Use infigraph get_doc_context/review directly, or use general-purpose agent." >&2
        ;;
    esac
    ;;
  Read)
    file_path=$(echo "$input" | jq -r '.tool_input.file_path // empty')
    if echo "$file_path" | grep -qE '\.(rs|ts|tsx|js|jsx|py|go|java|c|cpp|h|hpp|cs|rb|swift|kt)$'; then
      cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"INFIGRAPH HINT: You are reading a code file with Read. Prefer mcp__infigraph__get_doc_context (returns source+callers+callees) or mcp__infigraph__get_code_snippet for code understanding. Only use Read when you need exact line numbers for the Edit tool."}}
ENDJSON
    fi
    ;;
esac

exit 0
"#;

pub(crate) const SESSION_SAVE_HOOK_SCRIPT: &str = r#"#!/usr/bin/env bash
# Infigraph UserPromptSubmit hook — session save reminder
# Counts user exchanges per Claude session. Every 5th exchange, emits a
# reminder to call save_session. Resets when the PostToolUse reset hook fires.
# stdin: JSON {prompt, session_id, cwd, ...}

input=$(cat)
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Only enforce in Infigraph-indexed projects
[ -d "$cwd/.infigraph" ] || exit 0

session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -z "$session_id" ] && exit 0

counter_dir="${TMPDIR:-/tmp}/infigraph-sessions"
mkdir -p "$counter_dir" 2>/dev/null
counter_file="$counter_dir/$session_id.count"

count=0
[ -f "$counter_file" ] && count=$(cat "$counter_file" 2>/dev/null || echo 0)
count=$((count + 1))
echo "$count" > "$counter_file"

if [ $((count % 5)) -eq 0 ]; then
  cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"MANDATORY AUTO-SAVE: Print '💾 Auto-saving session...' to the user, then IMMEDIATELY call mcp__infigraph__save_session with summary, pending_tasks, decisions, and narrative from the current session. Do this BEFORE answering the user's question. After saving, print '✅ Session saved.' and continue with the user's request. This is automatic — do NOT ask permission, do NOT skip, do NOT defer."}}
ENDJSON
fi

exit 0
"#;

pub(crate) const SESSION_RESET_HOOK_SCRIPT: &str = r#"#!/usr/bin/env bash
# Infigraph PostToolUse hook — resets session save counter after save_session
# stdin: JSON {tool_name, tool_input, ...}

input=$(cat)
tool=$(echo "$input" | jq -r '.tool_name // empty')

[ "$tool" = "mcp__infigraph__save_session" ] || exit 0

session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -z "$session_id" ] && exit 0

counter_file="${TMPDIR:-/tmp}/infigraph-sessions/$session_id.count"
echo "0" > "$counter_file" 2>/dev/null

exit 0
"#;

pub(crate) const SESSION_START_HOOK_SCRIPT: &str = r#"#!/usr/bin/env bash
# Infigraph SessionStart hook — session continuity on startup/resume/compaction
# stdin: JSON {session_id, cwd, source, ...}
# source: "startup" | "resume" | "clear" | "compact"

input=$(cat)
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Only enforce in Infigraph-indexed projects
[ -d "$cwd/.infigraph" ] || exit 0

source_type=$(echo "$input" | jq -r '.source // "startup"')

case "$source_type" in
  compact)
    cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"INFIGRAPH SESSION SAVE (COMPACTION): Context was just compacted. Pre-compaction decisions and context are at risk of being lost. Call mcp__infigraph__save_session NOW with summary, pending_tasks, decisions, constraints, assumptions, and blockers from this session. Then call mcp__infigraph__get_latest_session to reload saved context. Do NOT skip this."}}
ENDJSON
    ;;
  startup|resume)
    cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"INFIGRAPH SESSION RESTORE: Call mcp__infigraph__get_latest_session to recover prior session context (decisions, constraints, blockers, pending tasks). Do NOT start work without checking prior session state."}}
ENDJSON
    ;;
  clear)
    # Reset exchange counters on /clear
    session_id=$(echo "$input" | jq -r '.session_id // empty')
    if [ -n "$session_id" ]; then
      echo "0" > "${TMPDIR:-/tmp}/infigraph-sessions/$session_id.count" 2>/dev/null
      echo "0" > "${TMPDIR:-/tmp}/claude-clear-suggest/$session_id.count" 2>/dev/null
    fi
    backup=$(ls -t "$cwd"/.infigraph/sessions/unsaved-transcript-*.md 2>/dev/null | head -1)
    if [ -n "$backup" ]; then
      cat <<ENDJSON
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"INFIGRAPH CONTEXT RESET: Context was cleared. A pre-clear transcript backup exists at $backup. Read this file (contains last ~5 exchanges as clean markdown), extract key context (summary, decisions, pending tasks, files touched), then call mcp__infigraph__save_session to persist it. After saving, delete the backup file with Bash rm. Then call mcp__infigraph__get_latest_session to reload. Do NOT proceed with user work until this recovery is complete."}}
ENDJSON
    else
      cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"INFIGRAPH CONTEXT RESET: Context was cleared. Call mcp__infigraph__get_latest_session to restore prior session state, then call mcp__infigraph__memory_context with the user's next query to inject relevant code and session context. Do NOT proceed without restoring context. NOTE: If session was NOT saved before /clear, some recent context may be lost — remind user to save_session before clearing next time."}}
ENDJSON
    fi
    ;;
esac

exit 0
"#;

pub(crate) const SESSION_END_SAVE_HOOK_SCRIPT: &str = r##"#!/usr/bin/env bash
# SessionEnd hook — extract last ~5 exchanges from transcript for recovery.
# Next SessionStart will detect this and prompt model to summarize + save_session.

input=$(cat)
reason=$(echo "$input" | jq -r '.reason // empty')
transcript_path=$(echo "$input" | jq -r '.transcript_path // empty')
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Only act for infigraph-indexed projects with a valid transcript
[ -d "$cwd/.infigraph" ] || exit 0
[ -f "$transcript_path" ] || exit 0

# Skip if save_session was called recently (counter reset to 0 means save just happened)
session_id=$(echo "$input" | jq -r '.session_id // empty')
if [ -n "$session_id" ]; then
  counter_file="${TMPDIR:-/tmp}/infigraph-sessions/$session_id.count"
  if [ -f "$counter_file" ]; then
    count=$(cat "$counter_file" 2>/dev/null || echo 0)
    [ "$count" -eq 0 ] && exit 0
  fi
fi

sessions_dir="$cwd/.infigraph/sessions"
mkdir -p "$sessions_dir"

backup="$sessions_dir/unsaved-transcript-${reason:-unknown}.md"

python3 -c "
import json, sys

messages = []
with open('$transcript_path') as f:
    for line in f:
        try:
            d = json.loads(line)
        except:
            continue
        if d.get('type') not in ('user', 'assistant'):
            continue
        role = d['type']
        msg = d.get('message', {})
        if role == 'user':
            content = msg.get('content', '')
            if isinstance(content, list):
                content = ' '.join(p.get('text','') for p in content if p.get('type')=='text')
            if content.strip():
                messages.append(('user', content.strip()))
        elif role == 'assistant':
            content = msg.get('content', '')
            if isinstance(content, list):
                parts = [p.get('text','') for p in content if p.get('type')=='text']
                content = ' '.join(parts)
            if content.strip():
                messages.append(('assistant', content.strip()))

recent = messages[-10:]
with open('$backup', 'w') as out:
    out.write('# Unsaved session context (last ~5 exchanges)\n\n')
    for role, text in recent:
        out.write(f'## {role.title()}\n{text}\n\n')
" 2>/dev/null

exit 0
"##;

pub(crate) const CLEAR_SUGGEST_HOOK_SCRIPT: &str = r#"#!/usr/bin/env bash
# Infigraph UserPromptSubmit hook — suggest /clear every 5 turns to keep context lean.
# Uses own counter file per session. Resets on session start (new session_id).

input=$(cat)
session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -z "$session_id" ] && exit 0

counter_dir="${TMPDIR:-/tmp}/claude-clear-suggest"
mkdir -p "$counter_dir" 2>/dev/null
counter_file="$counter_dir/$session_id.count"

count=0
[ -f "$counter_file" ] && count=$(cat "$counter_file" 2>/dev/null || echo 0)
count=$((count + 1))
echo "$count" > "$counter_file"

if [ $((count % 5)) -eq 0 ]; then
  cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"MANDATORY VISIBLE ACTION — CONTEXT CLEANUP: You MUST print this message verbatim to the user RIGHT NOW before doing anything else:\n\n---\n🧹 **Context getting long** — type `/clear` to reset. Session was auto-saved.\n---\n\nDo NOT skip this message. Do NOT silently absorb it. The user MUST see it in the chat output. After printing, continue with the user's request."}}
ENDJSON
fi

exit 0
"#;

pub fn allowed_tools() -> Vec<String> {
    infigraph_mcp::allowed_tools_from_names()
}

pub(crate) fn install_enforcement_hook(home: &std::path::Path) -> Result<()> {
    let hooks_dir = home.join(".claude").join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join("infigraph-enforce.sh");
    std::fs::write(&hook_path, ENFORCE_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("  Installed enforcement hook: {}", hook_path.display());

    let settings_path = home.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.is_file() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        json!({})
    };

    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    let hook_entry = json!({
        "matcher": "Grep|Glob|Bash|Agent|Read",
        "hooks": [{
            "type": "command",
            "command": hook_path.to_string_lossy(),
            "timeout": 5
        }]
    });

    let pre_tool = settings["hooks"]
        .get("PreToolUse")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let already_exists = pre_tool.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-enforce"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !already_exists {
        let mut arr = pre_tool;
        arr.push(hook_entry);
        settings["hooks"]["PreToolUse"] = serde_json::Value::Array(arr);

        let pretty = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&settings_path, pretty)?;
        println!("  Added PreToolUse hook to {}", settings_path.display());
    } else {
        let expected_matcher = "Grep|Glob|Bash|Agent|Read";
        let mut arr = pre_tool;
        let mut updated = false;
        for entry in arr.iter_mut() {
            let is_infigraph = entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hooks| {
                    hooks.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .map(|c| c.contains("infigraph-enforce"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            if is_infigraph {
                let current_matcher = entry.get("matcher").and_then(|m| m.as_str()).unwrap_or("");
                if current_matcher != expected_matcher {
                    entry["matcher"] = json!(expected_matcher);
                    updated = true;
                }
            }
        }
        if updated {
            settings["hooks"]["PreToolUse"] = serde_json::Value::Array(arr);
            let pretty = serde_json::to_string_pretty(&settings)?;
            std::fs::write(&settings_path, pretty)?;
            println!(
                "  Updated PreToolUse matcher in {}",
                settings_path.display()
            );
        } else {
            println!(
                "  PreToolUse hook already configured in {}",
                settings_path.display()
            );
        }
    }

    Ok(())
}

pub(crate) fn install_session_save_hook(home: &std::path::Path) -> Result<()> {
    let hooks_dir = home.join(".claude").join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;

    let save_hook_path = hooks_dir.join("infigraph-session-save.sh");
    std::fs::write(&save_hook_path, SESSION_SAVE_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&save_hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    let reset_hook_path = hooks_dir.join("infigraph-session-reset.sh");
    std::fs::write(&reset_hook_path, SESSION_RESET_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&reset_hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    let start_hook_path = hooks_dir.join("infigraph-session-start.sh");
    std::fs::write(&start_hook_path, SESSION_START_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&start_hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    println!(
        "  Installed session save hook: {}",
        save_hook_path.display()
    );
    println!(
        "  Installed session reset hook: {}",
        reset_hook_path.display()
    );
    println!(
        "  Installed session start hook: {}",
        start_hook_path.display()
    );

    let settings_path = home.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.is_file() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        json!({})
    };

    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    // UserPromptSubmit hook for session save reminder
    let save_hook_entry = json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": save_hook_path.to_string_lossy(),
            "timeout": 5
        }]
    });

    let user_prompt = settings["hooks"]
        .get("UserPromptSubmit")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let save_exists = user_prompt.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-session-save"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    let mut settings_changed = false;

    if !save_exists {
        let mut arr = user_prompt;
        arr.push(save_hook_entry);
        settings["hooks"]["UserPromptSubmit"] = serde_json::Value::Array(arr);
        settings_changed = true;
        println!(
            "  Added UserPromptSubmit hook to {}",
            settings_path.display()
        );
    } else {
        println!("  UserPromptSubmit session hook already configured");
    }

    // PostToolUse hook for counter reset
    let reset_hook_entry = json!({
        "matcher": "mcp__infigraph__save_session",
        "hooks": [{
            "type": "command",
            "command": reset_hook_path.to_string_lossy(),
            "timeout": 5,
            "async": true
        }]
    });

    let post_tool = settings["hooks"]
        .get("PostToolUse")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let reset_exists = post_tool.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-session-reset"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !reset_exists {
        let mut arr = post_tool;
        arr.push(reset_hook_entry);
        settings["hooks"]["PostToolUse"] = serde_json::Value::Array(arr);
        settings_changed = true;
        println!(
            "  Added PostToolUse reset hook to {}",
            settings_path.display()
        );
    } else {
        println!("  PostToolUse session reset hook already configured");
    }

    // SessionStart hook for compaction save + startup/resume restore
    let start_hook_entry = json!({
        "hooks": [{
            "type": "command",
            "command": start_hook_path.to_string_lossy(),
            "timeout": 5
        }]
    });

    let session_start = settings["hooks"]
        .get("SessionStart")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let start_exists = session_start.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-session-start"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !start_exists {
        let mut arr = session_start;
        arr.push(start_hook_entry);
        settings["hooks"]["SessionStart"] = serde_json::Value::Array(arr);
        settings_changed = true;
        println!("  Added SessionStart hook to {}", settings_path.display());
    } else {
        println!("  SessionStart session hook already configured");
    }

    if settings_changed {
        let pretty = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&settings_path, pretty)?;
    }

    Ok(())
}

pub(crate) fn install_clear_suggest_hook(home: &std::path::Path) -> Result<()> {
    let hooks_dir = home.join(".claude").join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join("infigraph-clear-suggest.sh");
    std::fs::write(&hook_path, CLEAR_SUGGEST_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("  Installed clear suggest hook: {}", hook_path.display());

    let settings_path = home.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.is_file() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        json!({})
    };

    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    let hook_entry = json!({
        "hooks": [{
            "type": "command",
            "command": hook_path.to_string_lossy(),
            "timeout": 5
        }]
    });

    let user_prompt = settings["hooks"]
        .get("UserPromptSubmit")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let already_exists = user_prompt.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-clear-suggest"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !already_exists {
        let mut arr = user_prompt;
        arr.push(hook_entry);
        settings["hooks"]["UserPromptSubmit"] = serde_json::Value::Array(arr);
        let pretty = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&settings_path, pretty)?;
        println!(
            "  Added UserPromptSubmit clear-suggest hook to {}",
            settings_path.display()
        );
    } else {
        println!("  UserPromptSubmit clear-suggest hook already configured");
    }

    Ok(())
}

pub(crate) fn install_session_end_hook(home: &std::path::Path) -> Result<()> {
    let hooks_dir = home.join(".claude").join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join("infigraph-session-end-save.sh");
    std::fs::write(&hook_path, SESSION_END_SAVE_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("  Installed session-end save hook: {}", hook_path.display());

    let settings_path = home.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.is_file() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        json!({})
    };

    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    let hook_entry = json!({
        "hooks": [{
            "type": "command",
            "command": hook_path.to_string_lossy(),
            "timeout": 10
        }]
    });

    let session_end = settings["hooks"]
        .get("SessionEnd")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let already_exists = session_end.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-session-end"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !already_exists {
        let mut arr = session_end;
        arr.push(hook_entry);
        settings["hooks"]["SessionEnd"] = serde_json::Value::Array(arr);
        let pretty = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&settings_path, pretty)?;
        println!("  Added SessionEnd hook to {}", settings_path.display());
    } else {
        println!("  SessionEnd session-end hook already configured");
    }

    Ok(())
}

pub(crate) fn install_claude_allowlist(home: &std::path::Path) -> Result<()> {
    let settings_path = home.join(".claude").join("settings.local.json");
    let mut settings: serde_json::Value = if settings_path.is_file() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    if settings.get("permissions").is_none() {
        settings["permissions"] = json!({});
    }
    let existing: Vec<String> = settings["permissions"]
        .get("allow")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let existing_set: std::collections::HashSet<&str> =
        existing.iter().map(|s| s.as_str()).collect();
    let mut allow_list = existing.clone();
    let mut added = 0usize;
    for tool in allowed_tools() {
        if !existing_set.contains(tool.as_str()) {
            allow_list.push(tool);
            added += 1;
        }
    }

    if added > 0 {
        settings["permissions"]["allow"] = serde_json::Value::Array(
            allow_list
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        );
        if let Some(parent) = settings_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pretty = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&settings_path, pretty)?;
        println!(
            "  Added {} Infigraph MCP tools to Claude Code allowlist ({})",
            added,
            settings_path.display()
        );
    } else {
        println!(
            "  Claude Code allowlist already up to date ({})",
            settings_path.display()
        );
    }

    Ok(())
}

pub(crate) fn uninstall_claude_allowlist(home: &std::path::Path) -> Result<()> {
    let settings_path = home.join(".claude").join("settings.local.json");
    if !settings_path.is_file() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&settings_path)?;
    let mut settings: serde_json::Value = serde_json::from_str(&content).unwrap_or(json!({}));

    let existing: Vec<String> = settings["permissions"]
        .get("allow")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let infigraph_tools = allowed_tools();
    let infigraph_set: std::collections::HashSet<&str> =
        infigraph_tools.iter().map(|s| s.as_str()).collect();
    let filtered: Vec<String> = existing
        .into_iter()
        .filter(|s| !infigraph_set.contains(s.as_str()))
        .collect();
    let removed = filtered.len()
        < settings["permissions"]["allow"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);

    if removed {
        settings["permissions"]["allow"] = serde_json::Value::Array(
            filtered
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        );
        let pretty = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&settings_path, pretty)?;
        println!(
            "  Removed Infigraph MCP tools from Claude Code allowlist ({})",
            settings_path.display()
        );
    }

    Ok(())
}

pub(crate) fn uninstall_hooks(home: &std::path::Path) -> Result<()> {
    let hooks_dir = home.join(".claude").join("hooks");
    for hook_file in &[
        "infigraph-enforce.sh",
        "infigraph-session-save.sh",
        "infigraph-session-reset.sh",
        "infigraph-session-start.sh",
        "infigraph-clear-suggest.sh",
        "infigraph-session-end-save.sh",
    ] {
        let hook_path = hooks_dir.join(hook_file);
        if hook_path.exists() {
            std::fs::remove_file(&hook_path)?;
            println!("  Removed hook: {}", hook_path.display());
        }
    }

    let settings_path = home.join(".claude").join("settings.json");
    if settings_path.is_file() {
        let content = std::fs::read_to_string(&settings_path)?;
        if let Ok(mut settings) = serde_json::from_str::<serde_json::Value>(&content) {
            let infigraph_hook = |entry: &serde_json::Value| -> bool {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains("infigraph-"))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            };
            for event in &[
                "PreToolUse",
                "UserPromptSubmit",
                "PostToolUse",
                "SessionStart",
                "SessionEnd",
            ] {
                if let Some(arr) = settings["hooks"]
                    .get_mut(*event)
                    .and_then(|v| v.as_array_mut())
                {
                    let before = arr.len();
                    arr.retain(|entry| !infigraph_hook(entry));
                    if arr.len() < before {
                        println!(
                            "  Removed {} hook(s) from {}",
                            event,
                            settings_path.display()
                        );
                    }
                }
            }
            let pretty = serde_json::to_string_pretty(&settings)?;
            std::fs::write(&settings_path, pretty)?;
        }
    }

    Ok(())
}
