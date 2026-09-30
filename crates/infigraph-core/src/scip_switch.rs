//! Whether implicit SCIP enrichment runs for a project: `[scip] enabled` in
//! `.infigraph/config.toml`, default on, overridden by `INFIGRAPH_SCIP_ENABLED`
//! (one `settings!` definition gives both). Off skips what `infigraph index`
//! and the daemon's staleness check would start on their own -- the detached
//! `scip-enrich` child, the foreground `--no-embed` pass, and the daemon's
//! in-process enrichment -- so a fixture that indexes a throwaway project
//! leaves no process behind to outlive it. The explicit commands
//! `scip-enrich` and `scip-import` ignore it: typing one means it.

use std::path::Path;

use crate::settings_file::ConfigScope;

const CATEGORY: &str = "scip";
const ENABLED: &str = "enabled";

crate::settings! {
    scip {
        enabled: crate::settings::Toggle = crate::settings::Toggle(true),
    }
}

/// Whether implicit SCIP enrichment runs for `root`. A value that does not
/// parse is warned about once and reads as the default (on).
pub fn scip_enabled(root: &Path) -> bool {
    Scip::resolve_or_default(RawScip::default(), ConfigScope::Project(root))
        .enabled
        .0
}

/// The env var that overrides [`scip_enabled`], derived from the settings
/// definition so a caller (a test fixture turning enrichment off) never
/// spells it as a literal.
pub fn enabled_env_name() -> String {
    crate::settings::env_name(CATEGORY, ENABLED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};

    #[test]
    fn enrichment_is_on_by_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(enabled_env_name());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        assert!(scip_enabled(tmp.path()));
    }

    #[test]
    fn the_project_config_turns_it_off() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(enabled_env_name());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        crate::settings_file::set_project_setting(
            tmp.path(),
            "scip",
            "enabled",
            toml_edit::value(false),
        )
        .unwrap();
        assert!(!scip_enabled(tmp.path()));
    }

    #[test]
    fn the_env_overrides_the_project_config() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        crate::settings_file::set_project_setting(
            tmp.path(),
            "scip",
            "enabled",
            toml_edit::value(true),
        )
        .unwrap();
        std::env::set_var(enabled_env_name(), "0");
        let off = scip_enabled(tmp.path());
        std::env::remove_var(enabled_env_name());
        assert!(!off);
    }

    #[test]
    fn the_env_name_comes_from_the_definition() {
        assert_eq!(enabled_env_name(), "INFIGRAPH_SCIP_ENABLED");
    }
}
