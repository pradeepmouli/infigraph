use anyhow::Result;
use serde_json::json;

pub fn allowed_tools() -> Vec<String> {
    infigraph_mcp::allowed_tools_from_names()
}

/// Plan the Claude Code permission allowlist: every Infigraph MCP tool in
/// `permissions.allow` of `~/.claude/settings.local.json`, other entries and
/// keys kept. Planned rather than written so `install --dry-run` previews it
/// with the artifacts (#170).
pub(crate) fn plan_claude_allowlist(
    home: &std::path::Path,
) -> Result<(std::path::PathBuf, crate::artifacts::Plan)> {
    use crate::artifacts::Plan;
    let settings_path = home.join(".claude").join("settings.local.json");
    let skip = |why: String| Plan::Skip {
        reason: format!(
            "{} {why} -- not adding the Infigraph tool allowlist",
            settings_path.display()
        ),
        manual_snippet: String::new(),
    };
    let raw = crate::artifacts::read_if_present(&settings_path)?;
    let mut settings: serde_json::Value = match &raw {
        None => json!({}),
        Some(bytes) => match serde_json::from_slice(bytes) {
            Ok(value) => value,
            // Rebuilding it from `{}`, as install once did, destroys the file.
            Err(e) => {
                let plan = skip(format!("is not valid JSON ({e})"));
                return Ok((settings_path, plan));
            }
        },
    };

    let existing: Vec<String> = settings
        .pointer("/permissions/allow")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let existing_set: std::collections::HashSet<&str> =
        existing.iter().map(|s| s.as_str()).collect();
    let missing: Vec<String> = allowed_tools()
        .into_iter()
        .filter(|tool| !existing_set.contains(tool.as_str()))
        .collect();
    if let (true, Some(raw)) = (missing.is_empty(), &raw) {
        // Byte-for-byte, so an up-to-date file is reported unchanged rather
        // than "updated" by re-serialization.
        return Ok((settings_path, Plan::Write(raw.clone())));
    }

    let Some(permissions) = settings
        .as_object_mut()
        .map(|root| root.entry("permissions").or_insert_with(|| json!({})))
        .and_then(|p| p.as_object_mut())
    else {
        let plan = skip("has no JSON object at `permissions`".to_string());
        return Ok((settings_path, plan));
    };
    let allow = existing.into_iter().chain(missing);
    permissions.insert(
        "allow".to_string(),
        serde_json::Value::Array(allow.map(serde_json::Value::String).collect()),
    );
    let pretty = serde_json::to_string_pretty(&settings)?;
    Ok((settings_path, Plan::Write(pretty.into_bytes())))
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
            // `settings["hooks"]` (Index, not get_mut) silently inserts a
            // null entry for a missing "hooks" key as a side effect of
            // indexing -- get_mut avoids that, since the artifact engine's
            // own settings.json removal (run_uninstall, ahead of this call)
            // has typically already deleted "hooks" entirely once every
            // event array it owned went empty.
            let mut changed = false;
            if let Some(hooks) = settings.get_mut("hooks") {
                for event in &[
                    "PreToolUse",
                    "UserPromptSubmit",
                    "PostToolUse",
                    "SessionStart",
                    "SessionEnd",
                    "PreCompact",
                ] {
                    if let Some(arr) = hooks.get_mut(*event).and_then(|v| v.as_array_mut()) {
                        let before = arr.len();
                        arr.retain(|entry| !infigraph_hook(entry));
                        if arr.len() < before {
                            changed = true;
                            println!(
                                "  Removed {} hook(s) from {}",
                                event,
                                settings_path.display()
                            );
                        }
                    }
                }
            }
            if changed {
                let pretty = serde_json::to_string_pretty(&settings)?;
                std::fs::write(&settings_path, pretty)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstall_hooks_does_not_inject_null_hooks_key_when_absent() {
        // Regression test: `settings["hooks"]` (Index, not get_mut) silently
        // inserts a null entry for a missing key as a side effect of
        // indexing. The artifact engine's own settings.json removal
        // (run_uninstall, which always runs before this call from
        // cmd_uninstall) typically deletes "hooks" entirely once every event
        // array it owned goes empty -- this must leave that absence alone,
        // not corrupt the file back to `{"hooks": null}`.
        let home_dir = tempfile::tempdir().unwrap();
        let claude_dir = home_dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join("settings.json"), "{}").unwrap();

        uninstall_hooks(home_dir.path()).unwrap();

        let content = std::fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            settings,
            json!({}),
            "settings.json should stay empty, not gain a null hooks key"
        );
    }

    #[test]
    fn uninstall_hooks_still_removes_hand_added_infigraph_hooks() {
        // The function's real remaining purpose: cleaning up infigraph hooks
        // a user (or an older install) added outside the artifact mechanism.
        let home_dir = tempfile::tempdir().unwrap();
        let claude_dir = home_dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let settings = json!({
            "hooks": {
                "PreToolUse": [
                    {"hooks": [{"type": "command", "command": "~/.claude/hooks/infigraph-enforce.sh"}]},
                    {"hooks": [{"type": "command", "command": "~/.claude/hooks/unrelated-tool.sh"}]}
                ]
            }
        });
        std::fs::write(
            claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        uninstall_hooks(home_dir.path()).unwrap();

        let content = std::fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        let written: serde_json::Value = serde_json::from_str(&content).unwrap();
        let pre_tool_use = written["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 1);
        assert!(pre_tool_use[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("unrelated-tool.sh"));
    }
}
