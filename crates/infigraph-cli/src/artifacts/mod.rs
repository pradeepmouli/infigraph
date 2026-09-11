//! Data-driven integration artifacts: bundled config fragments, hook scripts,
//! and docs for every supported agent/editor, applied via one of five
//! strategies. See docs/superpowers/specs/2026-08-09-agent-target-templates-design.md.
//!
//! Built bottom-up (Tasks 1-12 of the implementation plan): most items here
//! are only exercised by this module's own unit tests until discovery
//! (Task 9) and dispatch (Task 11) wire them together, and until
//! `cmd_install`/`cmd_uninstall` (Tasks 18-19) call into the module from
//! `install.rs`. Suppressed here rather than per-item; remove once real
//! non-test callers make it unnecessary.
#![allow(dead_code, unused_imports)]

use anyhow::Context;

mod convention;
mod discovery;
mod manifest;
mod ownership;
mod resolver;
mod step;
mod strategy;
mod template;

pub(crate) use discovery::{discover_artifacts, ResolvedArtifact};
pub(crate) use step::InstallStep;
pub(crate) use strategy::{ApplyOutcome, Strategy};

/// Outcome of removing one resolved artifact during uninstall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoveOutcome {
    /// The artifact was present and was removed.
    Removed,
    /// The artifact was never present (nothing to do).
    NotPresent,
    /// The artifact's `overwrite`-managed file was present but its content
    /// no longer matches what install wrote -- the user edited it, so it
    /// was left in place rather than silently deleted.
    PreservedModified,
}

include!(concat!(env!("OUT_DIR"), "/bundled_integrations.rs"));

pub(crate) fn apply_resolved_artifact(
    artifact: &ResolvedArtifact,
    home: &std::path::Path,
    mcp_path: &str,
    force: bool,
) -> anyhow::Result<ApplyOutcome> {
    let (target_path, resolved_content) = match &artifact.resolver {
        Some(spec) => {
            let output = resolver::run_resolver_from_script(
                &spec.script_bytes,
                &spec.script_filename,
                &spec.command_prefix,
                mcp_path,
                home,
            )
            .with_context(|| format!("running resolver {}", spec.script_relative_path))?;
            match output {
                resolver::ResolverOutput::Ok { data } => {
                    let content = match data.content {
                        Some(v) => Some(serde_json::to_vec(&v)?),
                        None => artifact.content.clone(),
                    };
                    (std::path::PathBuf::from(data.path), content)
                }
                resolver::ResolverOutput::Skip { message } => {
                    return Ok(ApplyOutcome::Skipped {
                        reason: format!("resolver reported skip: {message}"),
                        manual_snippet: String::new(),
                    });
                }
                resolver::ResolverOutput::Error { message } => {
                    anyhow::bail!("resolver {} failed: {message}", spec.script_relative_path);
                }
            }
        }
        None => {
            let relative = artifact.target_relative_path.as_ref().ok_or_else(|| {
                anyhow::anyhow!("artifact has neither a static path nor a resolver")
            })?;
            (home.join(relative), artifact.content.clone())
        }
    };

    match artifact.strategy {
        Strategy::JsonDeepMerge => {
            let content = resolved_content
                .ok_or_else(|| anyhow::anyhow!("json_deep_merge artifact has no content"))?;
            let text = std::str::from_utf8(&content)?;
            strategy::apply_json_deep_merge(&target_path, text)
        }
        Strategy::Overwrite => {
            let content = resolved_content
                .ok_or_else(|| anyhow::anyhow!("overwrite artifact has no content"))?;
            // Substitute install-time tokens ONCE, here, so the bytes the
            // ownership manifest hashes are the bytes actually on disk --
            // recording the raw bundled content instead would make every
            // token-carrying artifact read as "hand-edited" at uninstall
            // and be preserved forever (caught by
            // cmd_uninstall_removes_everything_cmd_install_wrote).
            let content = strategy::substitute_install_tokens(&content).into_owned();

            // Pre-write hand-edit guard: apply_overwrite used to write
            // unconditionally regardless of what was already on disk, so any
            // local change made to a hook (or other overwrite-strategy
            // artifact) since the last install was silently destroyed by the
            // next `infigraph install` -- with no warning, no diff, nothing.
            // The ownership manifest already existed for exactly this
            // comparison (it's what protects hand-edits on uninstall) but was
            // never consulted here, on the write side. --force bypasses this.
            //
            // A file already identical to what this install writes is not an
            // edit to protect, whatever the manifest says: that is a local fix
            // the bundled artifact has caught up to (#169). Writing it re-records
            // the hash, so the file is tracked as installed again rather than
            // skipped -- along with every later change to it -- forever.
            let already_current =
                std::fs::read(&target_path).is_ok_and(|on_disk| on_disk == content);
            if !force
                && !already_current
                && ownership::hand_edited_since_install(home, &target_path)?
            {
                return Ok(ApplyOutcome::Skipped {
                    reason: format!(
                        "{} was changed since infigraph last installed it -- not overwriting. Re-run with --force to overwrite anyway.",
                        target_path.display()
                    ),
                    manual_snippet: String::new(),
                });
            }

            let outcome = strategy::apply_overwrite(&target_path, &content)?;
            if matches!(outcome, ApplyOutcome::Written) {
                ownership::record_written(home, &target_path, &content)?;
                if target_path.components().any(|c| c.as_os_str() == "hooks") {
                    make_executable(&target_path)?;
                }
            }
            Ok(outcome)
        }
        Strategy::MarkerDelimited => {
            let content = resolved_content
                .ok_or_else(|| anyhow::anyhow!("marker_delimited artifact has no content"))?;
            let text = std::str::from_utf8(&content)?;
            let start = artifact
                .start
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("marker_delimited artifact missing start marker"))?;
            let end = artifact
                .end
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("marker_delimited artifact missing end marker"))?;
            strategy::apply_marker_delimited(&target_path, start, end, text)
        }
        Strategy::TomlSection => {
            let content = resolved_content
                .ok_or_else(|| anyhow::anyhow!("toml_section artifact has no content"))?;
            let text = std::str::from_utf8(&content)?;
            let key_path = artifact
                .key_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("toml_section artifact missing key_path"))?;
            strategy::apply_toml_section(&target_path, key_path, text)
        }
        Strategy::JsonKeyPath => {
            let content = resolved_content
                .ok_or_else(|| anyhow::anyhow!("json_key_path artifact has no content"))?;
            let text = std::str::from_utf8(&content)?;
            let key_path = artifact
                .key_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("json_key_path artifact missing key_path"))?;
            strategy::apply_json_key_path(&target_path, key_path, text)
        }
    }
}

/// Hook scripts run via a shebang through the user's shell, which requires
/// the executable bit -- unlike every other overwrite-strategy artifact
/// (settings.json fragments, docs), a non-executable hook script silently
/// never fires. No-op on non-Unix; Windows hook scripts aren't shipped.
#[cfg(unix)]
fn make_executable(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to make {} executable", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn remove_resolved_artifact(
    artifact: &ResolvedArtifact,
    home: &std::path::Path,
    mcp_path: &str,
) -> anyhow::Result<RemoveOutcome> {
    let target_path = match &artifact.resolver {
        Some(spec) => {
            let output = resolver::run_resolver_from_script(
                &spec.script_bytes,
                &spec.script_filename,
                &spec.command_prefix,
                mcp_path,
                home,
            )
            .with_context(|| format!("running resolver {}", spec.script_relative_path))?;
            match output {
                resolver::ResolverOutput::Ok { data } => std::path::PathBuf::from(data.path),
                resolver::ResolverOutput::Skip { .. } => return Ok(RemoveOutcome::NotPresent),
                resolver::ResolverOutput::Error { message } => {
                    anyhow::bail!("resolver {} failed: {message}", spec.script_relative_path);
                }
            }
        }
        None => {
            let relative = artifact.target_relative_path.as_ref().ok_or_else(|| {
                anyhow::anyhow!("artifact has neither a static path nor a resolver")
            })?;
            home.join(relative)
        }
    };

    let removed =
        match artifact.strategy {
            Strategy::JsonDeepMerge => {
                let content = artifact
                    .content
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("json_deep_merge artifact has no content"))?;
                let text = std::str::from_utf8(content)?;
                strategy::remove_json_deep_merge(&target_path, text)?
            }
            Strategy::Overwrite => {
                if !ownership::verify_unchanged_and_clear(home, &target_path)? {
                    return Ok(RemoveOutcome::PreservedModified);
                }
                strategy::remove_overwrite(&target_path)?
            }
            Strategy::MarkerDelimited => {
                let start = artifact.start.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("marker_delimited artifact missing start marker")
                })?;
                let end = artifact.end.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("marker_delimited artifact missing end marker")
                })?;
                strategy::remove_marker_delimited(&target_path, start, end)?
            }
            Strategy::TomlSection => {
                let key_path = artifact
                    .key_path
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("toml_section artifact missing key_path"))?;
                strategy::remove_toml_section(&target_path, key_path)?
            }
            Strategy::JsonKeyPath => {
                let key_path = artifact
                    .key_path
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("json_key_path artifact missing key_path"))?;
                strategy::remove_json_key_path(&target_path, key_path)?
            }
        };

    Ok(if removed {
        RemoveOutcome::Removed
    } else {
        RemoveOutcome::NotPresent
    })
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn full_discover_apply_verify_uninstall_verify_removed_cycle() {
        let bundled: &[(&str, &[u8])] = &[
            (
                "claude-code/.claude.json",
                br#"{"mcpServers":{"infigraph":{"command":"{{mcp_path}}","args":["--mcp"]}}}"#,
            ),
            (
                "claude-code/.claude/hooks/infigraph-enforce.sh",
                b"#!/usr/bin/env bash\necho enforce\n",
            ),
            (
                "claude-code/config.toml",
                br#"label = "Claude Code"

[[artifact]]
path = ".claude/CLAUDE.md"
strategy = "marker_delimited"
start = "<!-- infigraph-primary-search -->"
end = "<!-- /infigraph-primary-search -->"
content_file = "../shared/agents.md"
"#,
            ),
            (
                "shared/agents.md",
                b"## Infigraph instructions\nUse infigraph tools first.",
            ),
            (
                "codex/config.toml",
                br#"label = "Codex"

[[artifact]]
path = ".codex/config.toml"
strategy = "toml_section"
key_path = ["mcp_servers", "infigraph"]
content_file = "mcp-section.toml"
"#,
            ),
            (
                "codex/mcp-section.toml",
                b"command = \"{{mcp_path}}\"\nargs = [\"--mcp\"]",
            ),
        ];

        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let home = home_dir.path();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts = discover_artifacts(bundled, user_dir.path(), mcp_path).unwrap();
        assert_eq!(artifacts.len(), 4);

        for artifact in &artifacts {
            let outcome = apply_resolved_artifact(artifact, home, mcp_path, false).unwrap();
            assert!(
                matches!(outcome, ApplyOutcome::Written),
                "{:?} failed to apply",
                artifact.target_relative_path
            );
        }

        // Verify each landed at the expected real path with expected content --
        // .claude.json at $HOME directly (no .claude/ nesting, matching the
        // CLAUDE_CODE_SPECIAL destination it replaces), hooks nested under
        // .claude/hooks/ (stripped from the bundled claude-code/.claude/hooks/ tree).
        let claude_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(claude_json["mcpServers"]["infigraph"]["command"], mcp_path);

        let hook =
            std::fs::read_to_string(home.join(".claude/hooks/infigraph-enforce.sh")).unwrap();
        assert!(hook.contains("echo enforce"));

        let claude_md = std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();
        assert!(claude_md.contains("Use infigraph tools first."));

        let codex_toml = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(codex_toml.contains(mcp_path));
        assert!(codex_toml.contains("[mcp_servers.infigraph]"));

        // Reapply is idempotent (no duplication) -- exercises the whole
        // pipeline's self-healing property, not just one strategy in isolation.
        for artifact in &artifacts {
            apply_resolved_artifact(artifact, home, mcp_path, false).unwrap();
        }
        let claude_json_again: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            claude_json_again["mcpServers"]["infigraph"]["args"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // Uninstall every artifact, verify each is actually gone.
        for artifact in &artifacts {
            let removed = remove_resolved_artifact(artifact, home, mcp_path).unwrap();
            assert!(
                matches!(removed, RemoveOutcome::Removed),
                "{:?} was not removed",
                artifact.target_relative_path
            );
        }

        let claude_json_after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                .unwrap();
        assert!(claude_json_after["mcpServers"]["infigraph"].is_null());
        assert!(!home.join(".claude/hooks/infigraph-enforce.sh").exists());
        assert!(!std::fs::read_to_string(home.join(".claude/CLAUDE.md"))
            .unwrap()
            .contains("Use infigraph tools first."));
        assert!(!std::fs::read_to_string(home.join(".codex/config.toml"))
            .unwrap()
            .contains("[mcp_servers.infigraph]"));
    }

    #[test]
    fn user_override_end_to_end_replaces_bundled_content() {
        let bundled: &[(&str, &[u8])] = &[(
            "claude-code/.claude/hooks/infigraph-enforce.sh",
            b"#!/usr/bin/env bash\necho bundled\n",
        )];
        let user_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(user_dir.path().join("claude-code/.claude/hooks")).unwrap();
        std::fs::write(
            user_dir
                .path()
                .join("claude-code/.claude/hooks/infigraph-enforce.sh"),
            "#!/usr/bin/env bash\necho overridden\n",
        )
        .unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/bin/infigraph-mcp";

        let artifacts = discover_artifacts(bundled, user_dir.path(), mcp_path).unwrap();
        for artifact in &artifacts {
            apply_resolved_artifact(artifact, home_dir.path(), mcp_path, false).unwrap();
        }

        let content =
            std::fs::read_to_string(home_dir.path().join(".claude/hooks/infigraph-enforce.sh"))
                .unwrap();
        assert!(content.contains("echo overridden"));
    }

    #[test]
    fn resolver_driven_artifact_applies_to_resolver_computed_path() {
        let bundled: &[(&str, &[u8])] = &[
            (
                "zed/config.toml",
                br#"label = "Zed"

[[artifact]]
strategy = "json_key_path"
key_path = ["context_servers", "infigraph"]
resolver = ["./resolve-zed-path.sh"]
"#,
            ),
            (
                "zed/resolve-zed-path.sh",
                // Real resolvers (VS Code, Zed) compute a fully absolute
                // destination path themselves -- apply_resolved_artifact uses
                // `data.path` directly with no `home`-joining of its own.
                // This fixture mirrors that by reading "home" out of the
                // ResolverInput JSON on stdin and building an absolute path,
                // instead of returning a bare relative filename.
                b"#!/usr/bin/env bash\npython3 -c \"\nimport json, sys\nd = json.load(sys.stdin)\nout = {'status': 'ok', 'data': {'path': d['home'] + '/zed-settings.json', 'content': {'command': d['mcp_path'], 'args': ['--mcp'], 'env': {}}}}\nprint(json.dumps(out))\n\"\n",
            ),
        ];
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts = discover_artifacts(bundled, user_dir.path(), mcp_path).unwrap();
        assert_eq!(artifacts.len(), 1);

        let outcome =
            apply_resolved_artifact(&artifacts[0], home_dir.path(), mcp_path, false).unwrap();
        assert!(matches!(outcome, ApplyOutcome::Written));

        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join("zed-settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["context_servers"]["infigraph"]["command"], mcp_path);

        let removed = remove_resolved_artifact(&artifacts[0], home_dir.path(), mcp_path).unwrap();
        assert!(matches!(removed, RemoveOutcome::Removed));
        let after: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join("zed-settings.json")).unwrap(),
        )
        .unwrap();
        assert!(after["context_servers"]["infigraph"].is_null());
    }

    #[test]
    fn uninstall_preserves_a_hand_edited_overwrite_artifact_but_removes_an_unmodified_one() {
        let bundled: &[(&str, &[u8])] = &[
            (
                "claude-code/.claude/hooks/infigraph-enforce.sh",
                b"#!/usr/bin/env bash\necho enforce\n",
            ),
            (
                "cursor/.cursor/rules/infigraph.mdc",
                b"# Infigraph rules\nUse infigraph tools first.\n",
            ),
        ];
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let home = home_dir.path();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts = discover_artifacts(bundled, user_dir.path(), mcp_path).unwrap();
        assert_eq!(artifacts.len(), 2);

        for artifact in &artifacts {
            let outcome = apply_resolved_artifact(artifact, home, mcp_path, false).unwrap();
            assert!(matches!(outcome, ApplyOutcome::Written));
        }

        let hook_path = home.join(".claude/hooks/infigraph-enforce.sh");
        let rules_path = home.join(".cursor/rules/infigraph.mdc");

        // Simulate the user hand-editing the rules file after install, but
        // leaving the hook script untouched.
        std::fs::write(&rules_path, "# Infigraph rules\nI added my own note.\n").unwrap();

        for artifact in &artifacts {
            let outcome = remove_resolved_artifact(artifact, home, mcp_path).unwrap();
            let target = artifact.target_relative_path.as_deref().unwrap();
            if target.ends_with(".mdc") {
                assert_eq!(
                    outcome,
                    RemoveOutcome::PreservedModified,
                    "hand-edited rules file should have been preserved"
                );
            } else {
                assert_eq!(
                    outcome,
                    RemoveOutcome::Removed,
                    "unmodified hook script should have been removed"
                );
            }
        }

        assert!(!hook_path.exists(), "unmodified hook script should be gone");
        assert!(rules_path.exists(), "hand-edited rules file should remain");
        assert_eq!(
            std::fs::read_to_string(&rules_path).unwrap(),
            "# Infigraph rules\nI added my own note.\n"
        );
    }

    #[test]
    fn reinstall_skips_a_hand_edited_overwrite_artifact_but_force_overwrites_it() {
        // Regression test for the Aug-9 clobber incident: apply_overwrite used
        // to write unconditionally, silently destroying a hand-edited hook (or
        // any other overwrite-strategy artifact) on the next install. This
        // proves the pre-write guard actually protects the on-disk edit, and
        // that --force still allows an explicit overwrite.
        let bundled: &[(&str, &[u8])] = &[(
            "claude-code/.claude/hooks/infigraph-enforce.sh",
            b"#!/usr/bin/env bash\necho original\n",
        )];
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let home = home_dir.path();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts = discover_artifacts(bundled, user_dir.path(), mcp_path).unwrap();
        assert_eq!(artifacts.len(), 1);
        let artifact = &artifacts[0];

        let outcome = apply_resolved_artifact(artifact, home, mcp_path, false).unwrap();
        assert!(matches!(outcome, ApplyOutcome::Written));

        let hook_path = home.join(".claude/hooks/infigraph-enforce.sh");
        std::fs::write(
            &hook_path,
            "#!/usr/bin/env bash\necho my hand-tuned version\n",
        )
        .unwrap();

        // A plain reinstall must not clobber it.
        let outcome = apply_resolved_artifact(artifact, home, mcp_path, false).unwrap();
        assert!(
            matches!(outcome, ApplyOutcome::Skipped { .. }),
            "hand-edited hook should have been skipped, not overwritten: {outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&hook_path).unwrap(),
            "#!/usr/bin/env bash\necho my hand-tuned version\n",
            "hand-edited content must survive a non-forced reinstall"
        );

        // --force overrides the guard.
        let outcome = apply_resolved_artifact(artifact, home, mcp_path, true).unwrap();
        assert!(matches!(outcome, ApplyOutcome::Written));
        assert_eq!(
            std::fs::read_to_string(&hook_path).unwrap(),
            "#!/usr/bin/env bash\necho original\n",
            "force=true should overwrite back to the bundled content"
        );
    }

    /// #169: a local edit that the bundled artifact later catches up to is no
    /// longer an edit worth preserving. Without this, a fix made by hand and
    /// then upstreamed stays "hand-edited" forever -- the manifest still holds
    /// the pre-fix hash -- and every later install silently skips the file and
    /// every future change to it.
    #[test]
    fn a_hand_edit_the_bundled_artifact_catches_up_to_is_adopted_not_preserved() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let home = home_dir.path();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";
        let hook_path = home.join(".claude/hooks/infigraph-enforce.sh");
        let apply = |content: &'static [u8]| {
            let bundled: &[(&str, &[u8])] =
                &[("claude-code/.claude/hooks/infigraph-enforce.sh", content)];
            let artifacts = discover_artifacts(bundled, user_dir.path(), mcp_path).unwrap();
            apply_resolved_artifact(&artifacts[0], home, mcp_path, false).unwrap()
        };

        assert!(matches!(apply(b"echo v1\n"), ApplyOutcome::Written));
        std::fs::write(&hook_path, "echo v2\n").unwrap();

        // The new release ships exactly the hand-edited content.
        let outcome = apply(b"echo v2\n");
        assert!(
            matches!(outcome, ApplyOutcome::Written),
            "an on-disk file identical to what install writes has nothing to preserve: {outcome:?}"
        );

        // And it is tracked as installed again: a genuine edit from here on is
        // still protected, which only holds if the manifest now has v2's hash.
        std::fs::write(&hook_path, "echo v3 local\n").unwrap();
        assert!(matches!(apply(b"echo v2\n"), ApplyOutcome::Skipped { .. }));
        assert_eq!(
            std::fs::read_to_string(&hook_path).unwrap(),
            "echo v3 local\n"
        );
    }

    /// #168: tool-routing guidance has one source, `shared/tool-routing.md`,
    /// composed into every place an agent reads it: a skill where skills are
    /// supported, the always-loaded rules file where they are not. Before, the
    /// same guidance was hand-copied into three places and all three drifted.
    #[test]
    fn tool_routing_guidance_is_composed_from_one_source_into_every_agent() {
        let user_dir = tempfile::tempdir().unwrap();
        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), "/bin/infigraph-mcp")
                .unwrap();
        let content_of = |path: &str| {
            let artifact = artifacts
                .iter()
                .find(|a| a.target_relative_path.as_deref() == Some(path))
                .unwrap_or_else(|| panic!("no bundled artifact for {path}"));
            String::from_utf8(artifact.content.clone().unwrap()).unwrap()
        };
        let routing_heading = "## Infigraph — which tool answers which question";

        for skill in [
            ".claude/skills/infigraph-tool-routing/SKILL.md",
            ".codex/skills/infigraph-tool-routing/SKILL.md",
        ] {
            let body = content_of(skill);
            assert!(
                body.starts_with("---\nname: infigraph-tool-routing\n"),
                "{skill} must open with the skill's own frontmatter:\n{body}"
            );
            assert!(
                body.contains(routing_heading),
                "{skill} lacks the routing body"
            );
        }
        for rules in [
            ".cursor/rules/infigraph.mdc",
            ".windsurf/rules/infigraph.md",
        ] {
            let body = content_of(rules);
            assert!(
                body.contains("Primary Code Intelligence"),
                "{rules} lacks the instructions"
            );
            assert!(
                body.contains(routing_heading),
                "{rules} lacks the inlined routing body"
            );
        }
        // `infigraph init`'s project-level targets draw on the same files.
        assert!(crate::agent::infigraph_instructions().contains(routing_heading));

        let sources: Vec<&str> = BUNDLED_INTEGRATIONS
            .iter()
            .filter(|(_, bytes)| {
                std::str::from_utf8(bytes).is_ok_and(|text| text.contains(routing_heading))
            })
            .map(|(path, _)| *path)
            .collect();
        assert_eq!(
            sources,
            ["shared/tool-routing.md"],
            "the routing body must have one source"
        );
    }

    #[test]
    fn bundled_gemini_cli_mcp_fragment_applies_correctly() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let gemini = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".gemini/settings.json"))
            .expect("gemini-cli fragment should be discovered from the bundled registry");
        assert_eq!(gemini.strategy, Strategy::JsonDeepMerge);

        apply_resolved_artifact(gemini, home_dir.path(), mcp_path, false).unwrap();
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join(".gemini/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["mcpServers"]["infigraph"]["command"], mcp_path);
        assert_eq!(written["mcpServers"]["infigraph"]["args"][0], "--mcp");
    }

    #[test]
    fn bundled_opencode_mcp_fragment_applies_correctly() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let opencode = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".config/opencode/opencode.json"))
            .expect("opencode fragment should be discovered from the bundled registry");
        assert_eq!(opencode.strategy, Strategy::JsonDeepMerge);

        apply_resolved_artifact(opencode, home_dir.path(), mcp_path, false).unwrap();
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join(".config/opencode/opencode.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(written["mcp"]["infigraph"]["type"], "local");
        assert_eq!(written["mcp"]["infigraph"]["command"][0], mcp_path);
        assert_eq!(written["mcp"]["infigraph"]["command"][1], "--mcp");
    }

    #[test]
    fn bundled_aider_mcp_fragment_applies_correctly() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let aider = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".aider/mcp.json"))
            .expect("aider fragment should be discovered from the bundled registry");
        assert_eq!(aider.strategy, Strategy::JsonDeepMerge);

        apply_resolved_artifact(aider, home_dir.path(), mcp_path, false).unwrap();
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join(".aider/mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["mcpServers"]["infigraph"]["command"], mcp_path);
        assert_eq!(written["mcpServers"]["infigraph"]["args"][0], "--mcp");
    }

    #[test]
    fn bundled_kiro_mcp_fragment_applies_correctly() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let kiro = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".kiro/settings/mcp.json"))
            .expect("kiro fragment should be discovered from the bundled registry");
        assert_eq!(kiro.strategy, Strategy::JsonDeepMerge);

        apply_resolved_artifact(kiro, home_dir.path(), mcp_path, false).unwrap();
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join(".kiro/settings/mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["mcpServers"]["infigraph"]["command"], mcp_path);
        assert_eq!(written["mcpServers"]["infigraph"]["args"][0], "--mcp");
    }

    #[test]
    fn bundled_github_copilot_cli_mcp_fragment_applies_correctly() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let copilot = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".copilot/mcp-config.json"))
            .expect("github-copilot-cli fragment should be discovered from the bundled registry");
        assert_eq!(copilot.strategy, Strategy::JsonDeepMerge);

        apply_resolved_artifact(copilot, home_dir.path(), mcp_path, false).unwrap();
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join(".copilot/mcp-config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["mcpServers"]["infigraph"]["command"], mcp_path);
        assert_eq!(written["mcpServers"]["infigraph"]["args"][0], "--mcp");
        // Confirmed necessary via manual testing against real Copilot CLI in
        // upstream PR #56 (intuit/infigraph): without "type": "local",
        // Copilot CLI doesn't recognize this as a local MCP server; without
        // "tools": ["*"], it isn't granted tool access.
        assert_eq!(written["mcpServers"]["infigraph"]["type"], "local");
        assert_eq!(written["mcpServers"]["infigraph"]["tools"][0], "*");
    }

    #[test]
    fn bundled_claude_json_applies_correctly() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let claude_json = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".claude.json"))
            .expect("claude-code's .claude.json fragment should be discovered");
        assert_eq!(claude_json.strategy, Strategy::JsonDeepMerge);

        apply_resolved_artifact(claude_json, home_dir.path(), mcp_path, false).unwrap();
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["mcpServers"]["infigraph"]["command"], mcp_path);
    }

    #[test]
    fn bundled_claude_md_and_reindex_skill_apply_via_manifest() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();

        let claude_md = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".claude/CLAUDE.md"))
            .expect(
                "CLAUDE.md marker_delimited artifact should be discovered from claude-code/config.toml",
            );
        assert_eq!(claude_md.strategy, Strategy::MarkerDelimited);
        apply_resolved_artifact(claude_md, home_dir.path(), mcp_path, false).unwrap();
        let claude_md_content =
            std::fs::read_to_string(home_dir.path().join(".claude/CLAUDE.md")).unwrap();
        assert!(claude_md_content.contains("## Infigraph — Primary Code Intelligence"));
        assert!(claude_md_content.contains("<!-- infigraph-primary-search -->"));

        let skill = artifacts
            .iter()
            .find(|a| {
                a.target_relative_path.as_deref()
                    == Some(".claude/skills/infigraph-reindex/SKILL.md")
            })
            .expect("reindex skill artifact should be discovered from claude-code/config.toml");
        assert_eq!(skill.strategy, Strategy::Overwrite);
        apply_resolved_artifact(skill, home_dir.path(), mcp_path, false).unwrap();
        let skill_content = std::fs::read_to_string(
            home_dir
                .path()
                .join(".claude/skills/infigraph-reindex/SKILL.md"),
        )
        .unwrap();
        assert!(skill_content.starts_with("---\nname: infigraph-reindex"));
        assert!(skill_content.contains("mcp__infigraph__index_project"));
    }

    #[test]
    fn bundled_settings_json_multi_event_test() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let settings = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".claude/settings.json"))
            .expect("settings.json fragment should be discovered");
        assert_eq!(settings.strategy, Strategy::JsonDeepMerge);

        apply_resolved_artifact(settings, home_dir.path(), mcp_path, false).unwrap();
        apply_resolved_artifact(settings, home_dir.path(), mcp_path, false).unwrap(); // reapply: must not duplicate

        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();

        for (event, expected_count) in [
            ("PreToolUse", 1),
            ("PostToolUse", 5),
            ("UserPromptSubmit", 3),
            ("SessionStart", 1),
            ("SessionEnd", 1),
            ("PreCompact", 2),
        ] {
            let arr = written["hooks"][event]
                .as_array()
                .unwrap_or_else(|| panic!("{event} should be an array"));
            assert_eq!(
                arr.len(),
                expected_count,
                "{event} should have exactly {expected_count} entries after reapply, got {arr:?}"
            );
        }

        let enforce_command = written["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(enforce_command.contains("infigraph-enforce.sh"));
    }

    #[cfg(unix)]
    #[test]
    fn installed_hook_scripts_are_executable() {
        // Hook scripts run via a shebang through the user's shell -- unlike
        // every other overwrite-strategy artifact (settings.json fragments,
        // docs), a non-executable hook script silently never fires.
        use std::os::unix::fs::PermissionsExt;

        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let enforce = artifacts
            .iter()
            .find(|a| {
                a.target_relative_path.as_deref() == Some(".claude/hooks/infigraph-enforce.sh")
            })
            .unwrap();
        apply_resolved_artifact(enforce, home_dir.path(), mcp_path, false).unwrap();

        let installed = home_dir.path().join(".claude/hooks/infigraph-enforce.sh");
        let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o111,
            0o111,
            "installed hook script must be executable (mode {mode:o})"
        );
    }

    #[test]
    fn bundled_enforce_hook_allows_piped_grep_but_still_blocks_bare_grep() {
        // Regression test for the grep-piping detection bug caught live this
        // session: a real Bash command filtering another command's output
        // (`cmd 2>&1 | grep -iE "error"`, explicitly allowed by this repo's
        // own CLAUDE.md guidance) must NOT be blocked, while a bare/leading
        // grep call (a real code search) must still be blocked. This checks
        // the bundled content carries the fix's `cmd_without_piped_grep`
        // logic rather than the old blanket substring check.
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let enforce = artifacts
            .iter()
            .find(|a| {
                a.target_relative_path.as_deref() == Some(".claude/hooks/infigraph-enforce.sh")
            })
            .unwrap();
        apply_resolved_artifact(enforce, home_dir.path(), mcp_path, false).unwrap();
        let content =
            std::fs::read_to_string(home_dir.path().join(".claude/hooks/infigraph-enforce.sh"))
                .unwrap();
        assert!(
            content.contains("cmd_without_piped_grep"),
            "enforce.sh must carry the piped-grep exemption, not the old blanket check"
        );
        // Check the actual sed command line, not the whole file -- the fix's
        // own explanatory comment legitimately mentions "\b" as prose (why it
        // was avoided), so a whole-file substring check would false-positive
        // on that comment instead of testing the real regex.
        let sed_line = content
            .lines()
            .find(|line| line.contains("sed -E"))
            .expect("enforce.sh should contain the piped-grep sed substitution");
        assert!(
            !sed_line.contains("\\b"),
            "the sed substitution must not use \\b -- unsupported by macOS's BSD sed, \
             verified live during this fix (silently no-ops instead of erroring)"
        );
    }

    #[test]
    fn bundled_hook_scripts_have_expected_content() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        for (relative, expected_substring) in [
            (".claude/hooks/infigraph-enforce.sh", "deny-by-default"),
            (
                ".claude/hooks/infigraph-edit-tracker.sh",
                "recent_edits.log",
            ),
            (".claude/hooks/infigraph-session-save.sh", "save_session"),
            (".claude/hooks/infigraph-session-reset.sh", "save_session"),
            (
                ".claude/hooks/infigraph-session-start.sh",
                "inject_session_summary",
            ),
            (
                ".claude/hooks/infigraph-session-end-save.sh",
                "unsaved-transcript",
            ),
            (
                ".claude/hooks/infigraph-clear-suggest.sh",
                "save session and type",
            ),
            (
                ".claude/hooks/infigraph-clear-guard.sh",
                "Session not saved",
            ),
            (
                ".claude/hooks/infigraph-test-context-sentinel.sh",
                "generate_test_context",
            ),
            (
                ".claude/hooks/infigraph-search-fallback-sentinel.sh",
                "search-fallback-allowed",
            ),
        ] {
            let artifact = artifacts
                .iter()
                .find(|a| a.target_relative_path.as_deref() == Some(relative))
                .unwrap_or_else(|| panic!("{relative} should be discovered"));
            assert_eq!(artifact.strategy, Strategy::Overwrite);
            apply_resolved_artifact(artifact, home_dir.path(), mcp_path, false).unwrap();
            let content = std::fs::read_to_string(home_dir.path().join(relative)).unwrap();
            assert!(
                content.contains(expected_substring),
                "{relative} should contain {expected_substring:?}"
            );
        }
    }

    #[test]
    fn bundled_cursor_rules_and_mcp_apply_correctly() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();

        let rules = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".cursor/rules/infigraph.mdc"))
            .expect("cursor rules artifact should be discovered from cursor/config.toml");
        assert_eq!(rules.strategy, Strategy::Overwrite);
        apply_resolved_artifact(rules, home_dir.path(), mcp_path, false).unwrap();
        let rules_content =
            std::fs::read_to_string(home_dir.path().join(".cursor/rules/infigraph.mdc")).unwrap();
        assert!(rules_content.contains("## Infigraph — Primary Code Intelligence"));

        let mcp = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".cursor/mcp.json"))
            .expect("cursor mcp.json fragment should be discovered");
        assert_eq!(mcp.strategy, Strategy::JsonDeepMerge);
        apply_resolved_artifact(mcp, home_dir.path(), mcp_path, false).unwrap();
        let mcp_written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join(".cursor/mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(mcp_written["mcpServers"]["infigraph"]["command"], mcp_path);
    }

    #[test]
    fn bundled_windsurf_rules_and_mcp_apply_correctly() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();

        let rules = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".windsurf/rules/infigraph.md"))
            .expect("windsurf rules artifact should be discovered from windsurf/config.toml");
        assert_eq!(rules.strategy, Strategy::Overwrite);
        apply_resolved_artifact(rules, home_dir.path(), mcp_path, false).unwrap();
        let rules_content =
            std::fs::read_to_string(home_dir.path().join(".windsurf/rules/infigraph.md")).unwrap();
        assert!(rules_content.contains("## Infigraph — Primary Code Intelligence"));

        let mcp = artifacts
            .iter()
            .find(|a| {
                a.target_relative_path.as_deref() == Some(".codeium/windsurf/mcp_config.json")
            })
            .expect("windsurf mcp_config.json fragment should be discovered");
        assert_eq!(mcp.strategy, Strategy::JsonDeepMerge);
        apply_resolved_artifact(mcp, home_dir.path(), mcp_path, false).unwrap();
        let mcp_written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home_dir.path().join(".codeium/windsurf/mcp_config.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(mcp_written["mcpServers"]["infigraph"]["command"], mcp_path);
    }

    #[test]
    fn shared_agents_md_override_changes_both_cursor_and_windsurf_output() {
        let user_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(user_dir.path().join("shared")).unwrap();
        std::fs::write(
            user_dir.path().join("shared/agents.md"),
            "## Overridden instructions",
        )
        .unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        // The rules compose agents.md with the bundled routing body.
        const OVERRIDE_THEN_ROUTING: &str =
            "## Overridden instructions\n\n## Infigraph — which tool answers which question";

        let cursor_rules = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".cursor/rules/infigraph.mdc"))
            .unwrap();
        assert!(
            String::from_utf8(cursor_rules.content.clone().unwrap())
                .unwrap()
                .starts_with(OVERRIDE_THEN_ROUTING),
            "the override must lead the composed rules, followed by the routing body"
        );

        let windsurf_rules = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".windsurf/rules/infigraph.md"))
            .unwrap();
        assert!(
            String::from_utf8(windsurf_rules.content.clone().unwrap())
                .unwrap()
                .starts_with(OVERRIDE_THEN_ROUTING),
            "the override must lead the composed rules, followed by the routing body"
        );
    }

    #[test]
    fn bundled_codex_mcp_and_skill_apply_correctly() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();

        let mcp = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".codex/config.toml"))
            .expect("codex toml_section MCP artifact should be discovered");
        assert_eq!(mcp.strategy, Strategy::TomlSection);
        assert_eq!(
            mcp.key_path,
            Some(vec!["mcp_servers".to_string(), "infigraph".to_string()])
        );
        apply_resolved_artifact(mcp, home_dir.path(), mcp_path, false).unwrap();
        let toml_content =
            std::fs::read_to_string(home_dir.path().join(".codex/config.toml")).unwrap();
        assert!(toml_content.contains("[mcp_servers.infigraph]"));
        assert!(toml_content.contains(mcp_path));

        let skill = artifacts
            .iter()
            .find(|a| {
                a.target_relative_path.as_deref()
                    == Some(".codex/skills/infigraph-reindex/SKILL.md")
            })
            .expect("codex reindex skill artifact should be discovered");
        assert_eq!(skill.strategy, Strategy::Overwrite);
        apply_resolved_artifact(skill, home_dir.path(), mcp_path, false).unwrap();
        let skill_content = std::fs::read_to_string(
            home_dir
                .path()
                .join(".codex/skills/infigraph-reindex/SKILL.md"),
        )
        .unwrap();
        assert!(skill_content.starts_with("---\nname: infigraph-reindex"));
    }

    #[test]
    fn bundled_codex_toml_reapply_does_not_duplicate_section() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let mcp = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(".codex/config.toml"))
            .unwrap();

        apply_resolved_artifact(mcp, home_dir.path(), mcp_path, false).unwrap();
        apply_resolved_artifact(mcp, home_dir.path(), mcp_path, false).unwrap();

        let toml_content =
            std::fs::read_to_string(home_dir.path().join(".codex/config.toml")).unwrap();
        assert_eq!(toml_content.matches("[mcp_servers.infigraph]").count(), 1);
    }

    #[test]
    fn bundled_vscode_resolver_resolves_path_and_uses_local_content() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let vscode = artifacts
            .iter()
            .find(|a| a.integration_label == "VS Code")
            .expect("VS Code resolver artifact should be discovered");
        assert_eq!(vscode.strategy, Strategy::JsonDeepMerge);
        assert!(vscode.target_relative_path.is_none());
        assert!(vscode.resolver.is_some());

        let outcome = apply_resolved_artifact(vscode, home_dir.path(), mcp_path, false).unwrap();
        assert!(matches!(outcome, ApplyOutcome::Written));

        // The resolver branches on OS -- assert against whichever path it
        // actually resolved to for the OS running this test.
        let expected_suffix = match std::env::consts::OS {
            "macos" => "Library/Application Support/Code/User/mcp.json",
            "linux" => ".config/Code/User/mcp.json",
            "windows" => "AppData/Roaming/Code/User/mcp.json",
            other => panic!("unhandled test OS {other}"),
        };
        let path = home_dir.path().join(expected_suffix);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["servers"]["infigraph"]["command"], mcp_path);
    }

    #[test]
    fn bundled_zed_resolver_resolves_path_and_content() {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";

        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let zed = artifacts
            .iter()
            .find(|a| a.integration_label == "Zed")
            .expect("Zed resolver artifact should be discovered");
        assert_eq!(zed.strategy, Strategy::JsonKeyPath);
        assert_eq!(
            zed.key_path,
            Some(vec!["context_servers".to_string(), "infigraph".to_string()])
        );
        assert!(zed.target_relative_path.is_none());

        let outcome = apply_resolved_artifact(zed, home_dir.path(), mcp_path, false).unwrap();
        assert!(matches!(outcome, ApplyOutcome::Written));

        let expected_suffix = match std::env::consts::OS {
            "macos" => "Library/Application Support/Zed/settings.json",
            "linux" => ".config/zed/settings.json",
            "windows" => "AppData/Roaming/Zed/settings.json",
            other => panic!("unhandled test OS {other}"),
        };
        let path = home_dir.path().join(expected_suffix);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["context_servers"]["infigraph"]["command"], mcp_path);
        assert_eq!(written["context_servers"]["infigraph"]["args"][0], "--mcp");
    }

    /// #169: the bundled SessionEnd cleanup hook used to `kill -9` every
    /// infigraph-mcp that appeared after SessionStart -- including one a
    /// *different* session had just started, which killed that session's live
    /// MCP server mid-conversation. It must kill only processes owned by this
    /// session's own `claude` process (its child, or the worker one hop below)
    /// and leave an unrelated server alone.
    ///
    /// This test process stands in for `claude`: its pid is the recorded
    /// marker. Every kill the hook can make is confined to processes this test
    /// parented, so it is safe on a machine running real infigraph servers.
    #[cfg(unix)]
    #[test]
    fn bundled_session_cleanup_kills_only_this_sessions_mcp_processes() {
        if hook_tools_missing() {
            return;
        }
        let (_home, hook) = install_bundled_claude_hook("infigraph-process-cleanup.sh");

        // Its `ps` line matches the hook's pattern (`/infigraph-mcp --mcp`).
        let scratch = tempfile::tempdir().unwrap();
        let fake = fake_mcp_server(scratch.path());

        // This session's own server: a direct child of the stand-in `claude`.
        let mut ours = std::process::Command::new(&fake)
            .arg("--mcp")
            .spawn()
            .unwrap();
        // Another session's server: started from a shell that exits at once,
        // so it is reparented away from this process entirely.
        let unrelated_pid_file = scratch.path().join("unrelated.pid");
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "'{}' --mcp >/dev/null 2>&1 & echo $! > '{}'",
                fake.display(),
                unrelated_pid_file.display()
            ))
            .status()
            .unwrap();
        let unrelated: i32 = std::fs::read_to_string(&unrelated_pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;

        let tmp = tempfile::tempdir().unwrap();
        let track_dir = tmp.path().join("infigraph-proc-track");
        std::fs::create_dir_all(&track_dir).unwrap();
        std::fs::write(
            track_dir.join("t169.claude_pid"),
            std::process::id().to_string(),
        )
        .unwrap();

        let hook_status = run_hook(&hook, tmp.path(), r#"{"session_id":"t169"}"#).status;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let ours_gone = loop {
            if ours.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let unrelated_survived = alive(unrelated);

        // Clean up before asserting, so a failure cannot leak either process.
        let _ = ours.kill();
        let _ = ours.wait();
        unsafe { libc::kill(unrelated, libc::SIGKILL) };

        assert!(hook_status.success(), "the hook must exit 0");
        assert!(
            ours_gone,
            "this session's own MCP server (a child of the recorded claude pid) must be killed"
        );
        assert!(
            unrelated_survived,
            "a server this session does not own must never be killed"
        );
    }

    /// #168: the enforcement hook's denials name the tool that answers the
    /// question and never offer the agent a bypass it can write for itself --
    /// agents took that bypass as a routine workaround. And a command word
    /// the shell never runs, inside quotes or a heredoc body, must not block
    /// a command: that is how `gh issue create --title "..grep.."` and commit
    /// messages mentioning rg were refused.
    #[cfg(unix)]
    #[test]
    fn bundled_enforce_hook_routes_searches_and_ignores_unexecuted_text() {
        if hook_tools_missing() {
            return;
        }
        let (_home, hook) = install_bundled_claude_hook("infigraph-enforce.sh");
        // The hook allows everything while no infigraph-mcp process exists.
        let scratch = tempfile::tempdir().unwrap();
        let mut server = std::process::Command::new(fake_mcp_server(scratch.path()))
            .spawn()
            .unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".infigraph")).unwrap();
        let cwd = project.path().to_str().unwrap();

        // The denial reason, or None when the call is allowed.
        let decide = |tool: &str, tool_input: serde_json::Value| -> Option<String> {
            let input =
                serde_json::json!({"tool_name": tool, "cwd": cwd, "tool_input": tool_input});
            let out = run_hook(&hook, scratch.path(), &input.to_string());
            assert!(out.status.success(), "the hook must exit 0: {out:?}");
            let stdout = String::from_utf8(out.stdout).unwrap();
            (!stdout.trim().is_empty()).then(|| {
                let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
                v["hookSpecificOutput"]["permissionDecisionReason"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
        };
        let bash = |cmd: &str| decide("Bash", serde_json::json!({ "command": cmd }));
        let source = format!("{cwd}/src/lib.rs");

        // (case, decision, what a denial must name -- None means allowed)
        let decisions = [
            (
                "Grep",
                decide("Grep", serde_json::json!({"pattern": "foo"})),
                Some("regex=true"),
            ),
            ("bare grep", bash("grep -rn foo src"), Some("regex=true")),
            ("rg after &&", bash("cd src && rg foo"), Some("regex=true")),
            (
                "grep after a heredoc ends",
                bash("cat <<EOF > notes.txt\nrg is text here\nEOF\ngrep -rn foo src"),
                Some("regex=true"),
            ),
            (
                "find -name",
                bash("find . -name '*.rs'"),
                Some("list_files"),
            ),
            (
                "Glob",
                decide("Glob", serde_json::json!({"pattern": "**/*.rs"})),
                Some("list_files"),
            ),
            (
                "Read without offset",
                decide("Read", serde_json::json!({ "file_path": source })),
                Some("get_symbols_in_file"),
            ),
            ("piped filter", bash("cargo test 2>&1 | grep FAILED"), None),
            (
                "word in a quoted title",
                bash(r#"gh issue create --title "grep and rg are blocked" --body-file b.md"#),
                None,
            ),
            (
                "word in a heredoc body",
                bash("git commit -F - <<'EOF'\nfix: rg false positive\ngrep in the body\nEOF"),
                None,
            ),
            (
                "heredoc with quotes inside a quoted substitution",
                bash("git commit -m \"$(cat <<'EOF'\nsearch: \"grep\" quoted\nEOF\n)\""),
                None,
            ),
            (
                "word in a multi-line quoted message",
                bash("git commit -m \"subject\n\ngrep in the body\""),
                None,
            ),
            ("quoted find", bash("echo 'find . -name x'"), None),
            (
                "Read with offset",
                decide(
                    "Read",
                    serde_json::json!({ "file_path": source, "offset": 10 }),
                ),
                None,
            ),
        ];

        // Stop the stand-in before asserting, so a failure cannot leak it.
        let _ = server.kill();
        let _ = server.wait();

        for (case, decision, names) in decisions {
            match (decision, names) {
                (None, None) => {}
                (Some(reason), Some(names)) => {
                    assert!(
                        reason.contains(names),
                        "{case}: denial must name {names}: {reason}"
                    );
                    assert!(
                        reason.contains("infigraph-tool-routing")
                            && reason.contains("tell the user"),
                        "{case}: denial must point at the skill and the user: {reason}"
                    );
                    assert!(
                        !reason.contains(".search-fallback-allowed"),
                        "{case}: denial must not offer a self-written bypass: {reason}"
                    );
                }
                (decision, names) => {
                    panic!("{case}: expected denial naming {names:?}, got {decision:?}")
                }
            }
        }
    }

    /// Hook behaviour tests need the tools the bundled hooks call.
    #[cfg(unix)]
    fn hook_tools_missing() -> bool {
        let missing = ["jq", "pgrep"].into_iter().find(|tool| {
            std::process::Command::new(tool)
                .arg("--version")
                .output()
                .is_err()
        });
        if let Some(tool) = missing {
            eprintln!("skipping: the hook needs {tool}, which is not installed");
        }
        missing.is_some()
    }

    /// Install one bundled Claude Code hook into a fresh home directory. The
    /// returned directory must outlive the script path.
    #[cfg(unix)]
    fn install_bundled_claude_hook(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let user_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let mcp_path = "/opt/infigraph/bin/infigraph-mcp";
        let rel = format!(".claude/hooks/{name}");
        let artifacts =
            discover_artifacts(BUNDLED_INTEGRATIONS, user_dir.path(), mcp_path).unwrap();
        let artifact = artifacts
            .iter()
            .find(|a| a.target_relative_path.as_deref() == Some(rel.as_str()))
            .unwrap_or_else(|| panic!("{rel} is not a bundled artifact"));
        apply_resolved_artifact(artifact, home_dir.path(), mcp_path, false).unwrap();
        let path = home_dir.path().join(rel);
        (home_dir, path)
    }

    /// A stand-in MCP server: a script named `infigraph-mcp`, which is what
    /// the hooks' process checks look for. Short sleeps, so the child a
    /// SIGKILLed script leaves behind is gone within a second.
    #[cfg(unix)]
    fn fake_mcp_server(dir: &std::path::Path) -> std::path::PathBuf {
        let fake = dir.join("infigraph-mcp");
        std::fs::write(&fake, "#!/bin/sh\nwhile :; do sleep 1; done\n").unwrap();
        make_executable(&fake).unwrap();
        fake
    }

    /// Run a hook on `stdin` (its JSON input) with `TMPDIR` pointed at `tmp`.
    #[cfg(unix)]
    fn run_hook(
        hook: &std::path::Path,
        tmp: &std::path::Path,
        stdin: &str,
    ) -> std::process::Output {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = std::process::Command::new(hook)
            .env("TMPDIR", tmp)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}
