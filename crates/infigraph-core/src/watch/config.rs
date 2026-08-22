//! Persisted watch/watch-docs enable-disable policy.
//!
//! `write_watch_policy` and `watch_enabled` are the two halves of the same
//! policy: a CLI `infigraph watch enable|disable` (or `watch-docs
//! enable|disable`) call persists via `write_watch_policy`, and every
//! opportunistic auto-start call site reads the result back via
//! `watch_enabled` so the policy survives process restarts, not just the
//! current session. Both live here (not in `infigraph-cli` or
//! `infigraph-mcp`) so a future MCP-side `enable_watch`/`disable_watch` tool
//! can call the same write path the CLI already uses.
//!
//! Uses `toml_edit`'s `DocumentMut` rather than round-tripping through a
//! typed `serde` struct: `.infigraph/config.toml` is shared with unrelated
//! sections (e.g. `infigraph-mcp`'s own `[compression]` block) that this
//! crate has no schema for, so a surgical per-key edit is the only way to
//! avoid silently dropping fields this code doesn't know about.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::daemon_protocol::WatchRole;

fn section_for_role(role: WatchRole) -> Result<&'static str> {
    match role {
        WatchRole::Code => Ok("watch"),
        WatchRole::Docs => Ok("watch_docs"),
        WatchRole::Daemon => {
            anyhow::bail!("enable/disable has no meaning for role: Daemon")
        }
    }
}

/// Walk up from `std::env::current_dir()` looking for `.infigraph/config.toml`,
/// falling back to `$HOME/.infigraph/config.toml`. Deliberately duplicates
/// `infigraph-mcp/src/session_context.rs`'s `find_config_file_with_home_fallback`
/// (~15 lines) rather than depending on `infigraph-mcp` to reuse it -- that
/// crate depends on this one, not the other way around.
fn find_config_file_with_home_fallback() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let mut dir = cwd.as_path();
    loop {
        let candidate = dir.join(".infigraph").join("config.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let candidate = PathBuf::from(home).join(".infigraph").join("config.toml");
    candidate.exists().then_some(candidate)
}

/// Whether `section` ("watch" or "watch_docs") should auto-start, per the
/// persisted policy. Priority: env var (`INFIGRAPH_{SECTION}_ENABLED`) wins
/// over `config.toml`'s `[section].enabled`, which wins over the hardcoded
/// default (on). Mirrors `infigraph-mcp::session_context::
/// auto_start_watch_on_boot_enabled`'s exact precedence, generalized to any
/// section.
pub fn watch_enabled(section: &str) -> bool {
    let env_key = format!("INFIGRAPH_{}_ENABLED", section.to_uppercase());
    if let Ok(v) = std::env::var(&env_key) {
        return v != "0" && v.to_lowercase() != "false";
    }

    let Some(path) = find_config_file_with_home_fallback() else {
        return true;
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return true;
    };
    let Ok(doc) = contents.parse::<toml_edit::DocumentMut>() else {
        return true;
    };
    doc.get(section)
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// Persist `enabled` for `role` into `root/.infigraph/config.toml`, leaving
/// every other section (and any formatting/comments in sections this code
/// doesn't know about) untouched. `root` is always explicit here (unlike
/// `watch_enabled`'s cwd-walking discovery) -- `write_watch_policy`'s only
/// caller (`infigraph watch enable|disable`) always already has the
/// project root in hand.
pub fn write_watch_policy(root: &Path, role: WatchRole, enabled: bool) -> Result<()> {
    let section = section_for_role(role)?;
    let ig_dir = root.join(".infigraph");
    std::fs::create_dir_all(&ig_dir)?;
    let config_path = ig_dir.join("config.toml");

    let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&config_path)
        .unwrap_or_default()
        .parse()
        .unwrap_or_default();
    doc[section]["enabled"] = toml_edit::value(enabled);
    std::fs::write(&config_path, doc.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate process-global env vars -- `cargo test`
    /// runs unit tests in threads within one process, so two tests setting
    /// `INFIGRAPH_*_ENABLED` concurrently would race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn watch_enabled_env_override_priority() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_WATCH_ENABLED", "0");
        assert!(!watch_enabled("watch"));
        std::env::set_var("INFIGRAPH_WATCH_ENABLED", "false");
        assert!(!watch_enabled("watch"));
        std::env::set_var("INFIGRAPH_WATCH_ENABLED", "1");
        assert!(watch_enabled("watch"));
        std::env::remove_var("INFIGRAPH_WATCH_ENABLED");
    }

    #[test]
    fn watch_enabled_defaults_to_true_with_nothing_set() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_WATCH_ENABLED");
        std::env::remove_var("INFIGRAPH_WATCH_DOCS_ENABLED");
        assert!(watch_enabled("watch"));
        assert!(watch_enabled("watch_docs"));
    }

    #[test]
    fn write_watch_policy_rejects_daemon_role() {
        let tmp = tempfile::tempdir().unwrap();
        let err = write_watch_policy(tmp.path(), WatchRole::Daemon, true).unwrap_err();
        assert!(err.to_string().contains("Daemon"));
    }

    #[test]
    fn write_watch_policy_round_trips_code_and_docs_sections() {
        let tmp = tempfile::tempdir().unwrap();
        write_watch_policy(tmp.path(), WatchRole::Code, false).unwrap();
        write_watch_policy(tmp.path(), WatchRole::Docs, true).unwrap();

        let config_path = tmp.path().join(".infigraph").join("config.toml");
        let contents = std::fs::read_to_string(&config_path).unwrap();
        let doc: toml_edit::DocumentMut = contents.parse().unwrap();
        assert_eq!(doc["watch"]["enabled"].as_bool(), Some(false));
        assert_eq!(doc["watch_docs"]["enabled"].as_bool(), Some(true));
    }

    /// `write_watch_policy`'s surgical `toml_edit` write must not clobber
    /// unrelated sections it has no schema for -- e.g. `infigraph-mcp`'s own
    /// `[compression]` block, which this crate never parses into a typed
    /// struct. A naive "parse into ConfigFile, mutate, re-serialize the
    /// whole struct" approach would silently drop `[compression]` here.
    #[test]
    fn write_watch_policy_preserves_unrelated_compression_section() {
        let tmp = tempfile::tempdir().unwrap();
        let ig_dir = tmp.path().join(".infigraph");
        std::fs::create_dir_all(&ig_dir).unwrap();
        std::fs::write(
            ig_dir.join("config.toml"),
            "[compression]\nenabled = true\nlevel = \"aggressive\"\ntoken_budget = 12345\n",
        )
        .unwrap();

        write_watch_policy(tmp.path(), WatchRole::Code, false).unwrap();

        let contents = std::fs::read_to_string(ig_dir.join("config.toml")).unwrap();
        let doc: toml_edit::DocumentMut = contents.parse().unwrap();
        assert_eq!(doc["compression"]["enabled"].as_bool(), Some(true));
        assert_eq!(doc["compression"]["level"].as_str(), Some("aggressive"));
        assert_eq!(doc["compression"]["token_budget"].as_integer(), Some(12345));
        assert_eq!(doc["watch"]["enabled"].as_bool(), Some(false));
    }
}
