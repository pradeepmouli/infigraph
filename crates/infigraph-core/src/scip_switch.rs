//! Whether implicit SCIP enrichment runs for a project: `[scip] enabled` in
//! `.infigraph/config.toml`, default on, overridden by `INFIGRAPH_SCIP_ENABLED`
//! (one `settings!` definition gives both). Off skips what `infigraph index`
//! and the daemon's staleness check would start on their own -- the detached
//! `scip-enrich` child, the foreground `--no-embed` pass, and the daemon's
//! in-process enrichment -- so a fixture that indexes a throwaway project
//! leaves no process behind to outlive it. The explicit commands
//! `scip-enrich` and `scip-import` ignore it: typing one means it.
//!
//! `[scip] disable = ["python"]` is the same switch for one language: a
//! deny-list of language names (`INFIGRAPH_SCIP_DISABLE`, comma-separated),
//! read like `[ignore]` -- the project and user layers union and the
//! environment adds to them, because a list that only subtracts must not be
//! relaxable by a replacing variable. The implicit entry points ask
//! [`enabled_languages`] about the languages they detected, once, where they
//! detect them; an indexer is skipped only when every language it would
//! serve in this project is disabled. Indexing a language is not switched
//! here: `[ignore] code` with a glob already stops it, in walker and watcher
//! alike.

use std::collections::HashSet;
use std::path::Path;

use crate::ignore_rules::{layered_entries, EnvLayer, Trust};
use crate::settings::{LanguageList, Toggle};
use crate::settings_file::ConfigScope;

const CATEGORY: &str = "scip";
const ENABLED: &str = "enabled";
const DISABLE: &str = "disable";

crate::settings! {
    scip {
        enabled: Toggle = Toggle(true),
        disable: LanguageList = LanguageList(Vec::new()),
    }
}

/// Which layer stated a disabled language, for the report that names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Project,
    /// `~/.infigraph/config.toml` or this process's environment.
    UserOrEnvironment,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Layer::Project => "project config",
            Layer::UserOrEnvironment => "user config or environment",
        }
    }
}

/// A language `[scip] disable` names, and where it was stated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisabledLanguage {
    pub name: String,
    pub layer: Layer,
}

/// An indexer that still runs although some language it serves is disabled,
/// because another language it serves is present and enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StillServed {
    pub indexer: &'static str,
    pub disabled: Vec<String>,
}

/// The language names `[scip] disable` states, lowercased and without
/// repeats, from every layer.
pub fn disabled_languages(root: &Path) -> Vec<DisabledLanguage> {
    let entries = layered_entries(
        root,
        Scip::env_layer().scip_disable,
        EnvLayer::Adds,
        |item| match Scip::layers_disable(&[item]) {
            Ok(list) => list.unwrap_or_default(),
            Err(e) => {
                crate::settings::warn_once(&format!("{e}; ignoring it"));
                LanguageList::default()
            }
        },
    );
    let mut out: Vec<DisabledLanguage> = Vec::new();
    for (name, trust) in entries {
        if out.iter().all(|d| d.name != name) {
            out.push(DisabledLanguage {
                name,
                layer: match trust {
                    Trust::Project => Layer::Project,
                    Trust::User => Layer::UserOrEnvironment,
                },
            });
        }
    }
    out
}

/// Every language name some SCIP indexer serves: the values `[scip] disable`
/// can usefully hold.
pub fn known_languages() -> Vec<&'static str> {
    let mut known: Vec<&'static str> = crate::scip::INDEXER_LANGUAGES
        .iter()
        .flat_map(|(_, tags)| tags.iter().copied())
        .collect();
    known.sort_unstable();
    known
}

/// Names in `[scip] disable` that no indexer serves (a typo). They disable
/// nothing; `infigraph index` and `doctor` say so, and the degraded mode
/// `scip-unknown-language` shows it everywhere else.
pub fn unknown_languages(root: &Path) -> Vec<String> {
    let known = known_languages();
    disabled_languages(root)
        .into_iter()
        .map(|d| d.name)
        .filter(|name| !known.contains(&name.as_str()))
        .collect()
}

/// `detected` without the languages `[scip] disable` names: what an implicit
/// enrichment may run indexers for. Infallible, so a config edited under a
/// running daemon cannot take it down.
pub fn enabled_languages(root: &Path, detected: &HashSet<String>) -> HashSet<String> {
    let disabled = disabled_languages(root);
    detected
        .iter()
        .filter(|lang| disabled.iter().all(|d| d.name != **lang))
        .cloned()
        .collect()
}

/// The indexers that run for `detected` although a language they serve is
/// present and disabled (they serve another present, enabled one).
pub fn still_served(root: &Path, detected: &HashSet<String>) -> Vec<StillServed> {
    let enabled = enabled_languages(root, detected);
    let mut out = Vec::new();
    for (indexer, tags) in crate::scip::INDEXER_LANGUAGES {
        if !tags.iter().any(|t| enabled.contains(*t)) {
            continue;
        }
        let disabled: Vec<String> = tags
            .iter()
            .filter(|t| detected.contains(**t) && !enabled.contains(**t))
            .map(|t| t.to_string())
            .collect();
        if !disabled.is_empty() {
            out.push(StillServed { indexer, disabled });
        }
    }
    out
}

/// The env var that adds to [`disabled_languages`], derived from the
/// settings definition.
pub fn disable_env_name() -> String {
    crate::settings::env_name(CATEGORY, DISABLE)
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

#[cfg(test)]
mod disable_tests {
    use super::*;
    use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};
    use std::collections::HashSet;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn project_disable(root: &Path, names: &[&str]) {
        let mut array = toml_edit::Array::new();
        for name in names {
            array.push(*name);
        }
        crate::settings_file::set_project_setting(root, "scip", "disable", toml_edit::value(array))
            .unwrap();
    }

    fn user_config(text: &str) {
        let dir = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".infigraph");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    fn names(root: &Path) -> Vec<String> {
        disabled_languages(root)
            .into_iter()
            .map(|d| d.name)
            .collect()
    }

    #[test]
    fn nothing_is_disabled_by_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(disable_env_name());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        assert!(names(tmp.path()).is_empty());
        assert!(unknown_languages(tmp.path()).is_empty());
        let detected = set(&["python", "rust"]);
        assert_eq!(enabled_languages(tmp.path(), &detected), detected);
    }

    #[test]
    fn the_project_config_disables_a_language_by_any_case() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(disable_env_name());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        project_disable(tmp.path(), &["Python"]);

        assert_eq!(names(tmp.path()), ["python"]);
        assert_eq!(
            disabled_languages(tmp.path())[0].layer,
            Layer::Project,
            "the report names the layer"
        );
        assert_eq!(
            enabled_languages(tmp.path(), &set(&["python", "rust"])),
            set(&["rust"])
        );
        assert!(scip_enabled(tmp.path()), "the master switch is untouched");
    }

    #[test]
    fn the_project_and_user_layers_union_and_the_environment_adds() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        user_config("[scip]\ndisable = [\"go\"]\n");
        let tmp = tempfile::tempdir().unwrap();
        project_disable(tmp.path(), &["python"]);
        std::env::set_var(disable_env_name(), "Ruby, ,php");

        let got = names(tmp.path());
        std::env::remove_var(disable_env_name());

        for name in ["go", "python", "ruby", "php"] {
            assert!(got.iter().any(|n| n == name), "{name} missing from {got:?}");
        }
        assert_eq!(got.len(), 4, "{got:?}");
    }

    #[test]
    fn an_empty_environment_variable_does_not_relax_the_files() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        project_disable(tmp.path(), &["python"]);
        std::env::set_var(disable_env_name(), "");

        let got = names(tmp.path());
        std::env::remove_var(disable_env_name());

        assert_eq!(got, ["python"]);
    }

    #[test]
    fn a_lone_string_is_a_mistake_that_disables_nothing_and_does_not_panic() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(disable_env_name());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        crate::settings_file::set_project_setting(
            tmp.path(),
            "scip",
            "disable",
            toml_edit::value("python"),
        )
        .unwrap();

        assert!(names(tmp.path()).is_empty());
    }

    #[test]
    fn a_name_no_indexer_serves_is_unknown_and_is_listed_with_the_known_ones() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(disable_env_name());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        project_disable(tmp.path(), &["pyton", "python"]);

        assert_eq!(unknown_languages(tmp.path()), ["pyton"]);
        let known = known_languages();
        for name in ["python", "tsx", "c_sharp", "rust"] {
            assert!(known.contains(&name), "{name} not in {known:?}");
        }
        // The unknown one disables nothing; the right one still does.
        assert_eq!(
            enabled_languages(tmp.path(), &set(&["python", "rust"])),
            set(&["rust"])
        );
    }

    /// The catalog table, spelled out (the test pins what the shared
    /// indexers serve).
    #[test]
    fn the_indexers_serve_these_languages() {
        let table: Vec<(&str, Vec<&str>)> = crate::scip::INDEXER_LANGUAGES
            .iter()
            .map(|(binary, tags)| (*binary, tags.to_vec()))
            .collect();
        assert_eq!(
            table,
            vec![
                (
                    "scip-typescript",
                    vec!["typescript", "javascript", "tsx", "jsx"]
                ),
                ("scip-python", vec!["python"]),
                ("rust-analyzer", vec!["rust"]),
                ("scip-java", vec!["java", "kotlin", "scala"]),
                ("scip-go", vec!["go"]),
                ("scip-ruby", vec!["ruby"]),
                ("scip-dotnet", vec!["c_sharp", "fsharp"]),
                ("scip-clang", vec!["c", "cpp"]),
                ("scip-dart", vec!["dart"]),
                ("scip-php", vec!["php"]),
            ]
        );
    }

    /// An indexer is skipped only when every language it would serve in THIS
    /// project is disabled.
    #[test]
    fn a_shared_indexer_still_runs_while_one_of_its_languages_is_enabled() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(disable_env_name());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        project_disable(tmp.path(), &["tsx"]);
        let detected = set(&["typescript", "tsx", "python"]);

        let enabled = enabled_languages(tmp.path(), &detected);
        assert!(enabled.contains("typescript"), "scip-typescript still runs");
        assert!(!enabled.contains("tsx"));
        assert_eq!(
            still_served(tmp.path(), &detected),
            vec![StillServed {
                indexer: "scip-typescript",
                disabled: vec!["tsx".to_string()],
            }],
            "doctor can say scip-typescript still runs for tsx"
        );

        project_disable(tmp.path(), &["tsx", "typescript"]);
        let enabled = enabled_languages(tmp.path(), &detected);
        assert_eq!(
            enabled,
            set(&["python"]),
            "every present language of it is off"
        );
        assert!(still_served(tmp.path(), &detected).is_empty());
    }

    #[test]
    fn the_env_name_comes_from_the_definition() {
        assert_eq!(disable_env_name(), "INFIGRAPH_SCIP_DISABLE");
    }
}
