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

/// Whether `infigraph index` starts SCIP enrichment for `root` when it
/// finishes. A linked worktree skips it on an incremental index:
/// `worktree init` copied the main checkout's already-enriched graph, the
/// indexers re-index the whole project for any change, and the daemon's
/// staleness check enriches the worktree once it has drifted far enough. A
/// full rebuild starts from an empty graph that the staleness check would
/// never enrich (it skips a graph never enriched), so it still does.
pub fn index_enriches(root: &Path, full: bool) -> bool {
    scip_enabled(root) && (full || !crate::worktree::is_linked_worktree(root))
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

#[cfg(test)]
mod index_enriches_tests {
    use super::*;
    use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};

    #[test]
    fn a_linked_worktree_skips_enrichment_on_an_incremental_index_only() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(enabled_env_name());
        let _home = PinnedHome::empty();
        let (_tmp, main, linked) = crate::worktree::tests::repo_with_linked_worktree();
        assert!(!index_enriches(&linked, false), "incremental in a worktree");
        assert!(
            index_enriches(&linked, true),
            "a full rebuild still enriches"
        );
        assert!(
            index_enriches(&main, false),
            "the main checkout is unchanged"
        );
    }

    #[test]
    fn the_switch_still_turns_it_all_off() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(enabled_env_name(), "0");
        let full = index_enriches(tmp.path(), true);
        std::env::remove_var(enabled_env_name());
        assert!(!full);
    }
}
