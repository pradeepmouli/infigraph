//! Persisted watch/watch-docs enable-disable policy.
//!
//! `write_watch_policy` and `watch_enabled_at` are the two halves of the
//! same policy: a CLI `infigraph watch enable|disable` (or `watch-docs
//! enable|disable`) call persists via `write_watch_policy`, and every
//! opportunistic auto-start call site reads the result back via
//! `watch_enabled_at` so the policy survives process restarts, not just the
//! current session. Both live here (not in `infigraph-cli` or
//! `infigraph-mcp`) so a future MCP-side `enable_watch`/`disable_watch` tool
//! can call the same write path the CLI already uses.
//!
//! Both halves are keyed by an explicit project root, not by
//! `std::env::current_dir()`: every caller already has the target project's
//! (canonicalized) root in hand, and a daemon or MCP server's cwd routinely
//! differs from the project it's serving. A cwd-based lookup would read a
//! different `config.toml` than `write_watch_policy` wrote (or silently
//! fall back to `$HOME/.infigraph/config.toml`, disabling watching
//! machine-wide with no per-project override).
//!
//! Uses `toml_edit`'s `DocumentMut` rather than round-tripping through a
//! typed `serde` struct: `.infigraph/config.toml` is shared with unrelated
//! sections (e.g. `infigraph-mcp`'s own `[compression]` block) that this
//! crate has no schema for, so a surgical per-key edit is the only way to
//! avoid silently dropping fields this code doesn't know about.

use std::path::Path;

use anyhow::{Context, Result};

use crate::daemon_protocol::{write_atomic, WatchRole};

fn section_for_role(role: WatchRole) -> Result<&'static str> {
    match role {
        WatchRole::Code => Ok("watch"),
        WatchRole::Docs => Ok("watch_docs"),
        WatchRole::Daemon => {
            anyhow::bail!("enable/disable has no meaning for role: Daemon")
        }
    }
}

/// Whether `section` ("watch" or "watch_docs") should auto-start for the
/// project rooted at `root`, per the persisted policy. Priority: env var
/// (`INFIGRAPH_{SECTION}_ENABLED`) wins over `root/.infigraph/config.toml`'s
/// `[section].enabled`, which wins over the hardcoded default (on). Mirrors
/// `infigraph-mcp::session_context::auto_start_watch_on_boot_enabled`'s
/// exact precedence, generalized to any section and any root.
pub fn watch_enabled_at(root: &Path, section: &str) -> bool {
    let env_key = format!("INFIGRAPH_{}_ENABLED", section.to_uppercase());
    if let Ok(v) = std::env::var(&env_key) {
        return v != "0" && v.to_lowercase() != "false";
    }

    let config_path = root.join(".infigraph").join("config.toml");
    let Ok(contents) = std::fs::read_to_string(&config_path) else {
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
/// doesn't know about) untouched.
///
/// A missing config file defaults to a fresh, empty document (there is
/// nothing to preserve). A *present but unreadable or unparsable* file is a
/// different situation -- silently defaulting there would clobber the rest
/// of the user's config (e.g. `infigraph-mcp`'s `[compression]` block) the
/// moment it has a syntax error, which is exactly the kind of destructive
/// "recovery" this function must not do. So only the missing-file case
/// defaults; a present-but-broken file makes this call fail loudly instead.
pub fn write_watch_policy(root: &Path, role: WatchRole, enabled: bool) -> Result<()> {
    let section = section_for_role(role)?;
    let ig_dir = root.join(".infigraph");
    std::fs::create_dir_all(&ig_dir)?;
    let config_path = ig_dir.join("config.toml");

    let mut doc: toml_edit::DocumentMut = match std::fs::read_to_string(&config_path) {
        Ok(contents) => contents
            .parse()
            .with_context(|| format!("{} contains invalid TOML", config_path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml_edit::DocumentMut::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", config_path.display()));
        }
    };
    doc[section]["enabled"] = toml_edit::value(enabled);
    write_atomic(&config_path, &doc.to_string())?;
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
    fn watch_enabled_at_env_override_priority() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("INFIGRAPH_WATCH_ENABLED", "0");
        assert!(!watch_enabled_at(tmp.path(), "watch"));
        std::env::set_var("INFIGRAPH_WATCH_ENABLED", "false");
        assert!(!watch_enabled_at(tmp.path(), "watch"));
        std::env::set_var("INFIGRAPH_WATCH_ENABLED", "1");
        assert!(watch_enabled_at(tmp.path(), "watch"));
        std::env::remove_var("INFIGRAPH_WATCH_ENABLED");
    }

    #[test]
    fn watch_enabled_at_defaults_to_true_with_nothing_set() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var("INFIGRAPH_WATCH_ENABLED");
        std::env::remove_var("INFIGRAPH_WATCH_DOCS_ENABLED");
        assert!(watch_enabled_at(tmp.path(), "watch"));
        assert!(watch_enabled_at(tmp.path(), "watch_docs"));
    }

    /// Regression for the read/write root mismatch: `watch_enabled_at` must
    /// read the policy from the *given* root and must not be fooled by an
    /// unrelated directory's `config.toml` -- otherwise an MCP server or
    /// daemon whose cwd differs from the project it's serving could read a
    /// different (or nonexistent) config file than `write_watch_policy`
    /// wrote. Deliberately does not mutate `current_dir()` (a process-global,
    /// shared across every test thread) -- `watch_enabled_at` no longer
    /// consults it at all, so isolation is proven by giving two distinct
    /// roots conflicting policies and checking each root's read stays
    /// pinned to its own root.
    #[test]
    fn watch_enabled_at_reads_given_root_not_an_unrelated_directory() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_WATCH_ENABLED");
        let project = tempfile::tempdir().unwrap();
        let decoy = tempfile::tempdir().unwrap();
        write_watch_policy(project.path(), WatchRole::Code, false).unwrap();
        write_watch_policy(decoy.path(), WatchRole::Code, true).unwrap();

        assert!(!watch_enabled_at(project.path(), "watch"));
        assert!(watch_enabled_at(decoy.path(), "watch"));
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

    /// A `config.toml` with a TOML syntax error must make `write_watch_policy`
    /// fail loudly, not silently replace the file with a fresh document
    /// containing only the watch section -- that would destroy every other
    /// section (e.g. `[compression]`) the moment the file has a typo.
    #[test]
    fn write_watch_policy_rejects_malformed_config_instead_of_clobbering_it() {
        let tmp = tempfile::tempdir().unwrap();
        let ig_dir = tmp.path().join(".infigraph");
        std::fs::create_dir_all(&ig_dir).unwrap();
        let config_path = ig_dir.join("config.toml");
        let malformed = "[compression\nenabled = true\n";
        std::fs::write(&config_path, malformed).unwrap();

        let err = write_watch_policy(tmp.path(), WatchRole::Code, false).unwrap_err();
        assert!(err.to_string().contains("invalid TOML"), "{err}");

        // The malformed file must be left exactly as it was -- no partial
        // or "recovered" rewrite.
        let contents = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(contents, malformed);
    }

    /// A missing `config.toml` is the one case that legitimately defaults
    /// to a fresh document -- there's nothing to preserve or fail on.
    #[test]
    fn write_watch_policy_creates_fresh_config_when_none_exists() {
        let tmp = tempfile::tempdir().unwrap();
        write_watch_policy(tmp.path(), WatchRole::Code, true).unwrap();
        let config_path = tmp.path().join(".infigraph").join("config.toml");
        let contents = std::fs::read_to_string(&config_path).unwrap();
        let doc: toml_edit::DocumentMut = contents.parse().unwrap();
        assert_eq!(doc["watch"]["enabled"].as_bool(), Some(true));
    }

    /// `write_watch_policy` must route through `write_atomic` (temp file +
    /// rename), not a direct `std::fs::write` -- no stray `.tmp-*` sibling
    /// should survive a successful write.
    #[test]
    fn write_watch_policy_leaves_no_temp_file_behind() {
        let tmp = tempfile::tempdir().unwrap();
        write_watch_policy(tmp.path(), WatchRole::Code, true).unwrap();
        let ig_dir = tmp.path().join(".infigraph");
        let leftover: Vec<_> = std::fs::read_dir(&ig_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftover.is_empty(), "leftover temp files: {leftover:?}");
    }
}
