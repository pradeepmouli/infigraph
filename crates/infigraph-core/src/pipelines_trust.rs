//! Which projects may run their own pipeline plugins.
//!
//! A pipeline plugin is a command. User-level plugins (`~/.infigraph/pipelines`)
//! belong to the person running infigraph. Project-level plugins
//! (`<project>/pipelines`) arrive with the repository, so running them as
//! documents are indexed would execute whatever a cloned repo names. They run
//! only for a project the user has listed in `[pipelines] trusted_projects`.
//!
//! The list is read from the user layer only (`~/.infigraph/config.toml`, or
//! `INFIGRAPH_PIPELINES_TRUSTED_PROJECTS`, comma-separated): a project's own
//! `.infigraph/config.toml` can never vouch for itself.

use std::path::{Path, PathBuf};

use crate::settings::PathList;
use crate::settings_file::ConfigScope;

const CATEGORY: &str = "pipelines";
const TRUSTED_PROJECTS: &str = "trusted_projects";

crate::settings! {
    pipelines {
        // Absolute project roots whose own `<project>/pipelines` plugins may
        // run. Relative entries are ignored: their meaning would depend on
        // the working directory.
        trusted_projects: PathList = PathList(Vec::new()),
    }
}

/// The trusted project roots, canonicalized, from the user layer only.
pub fn trusted_projects() -> Vec<PathBuf> {
    Pipelines::resolve_or_default(RawPipelines::default(), ConfigScope::User)
        .trusted_projects
        .0
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| crate::project::canonicalize_lenient(&p))
        .collect()
}

/// Whether `root`'s own pipeline plugins may run.
pub fn is_trusted_project(root: &Path) -> bool {
    let root = crate::project::canonicalize_lenient(root);
    trusted_projects().contains(&root)
}

/// The env var that overrides `[pipelines] trusted_projects`, derived from the
/// settings definition.
pub fn trusted_projects_env_name() -> String {
    crate::settings::env_name(CATEGORY, TRUSTED_PROJECTS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};

    fn write_user_config(body: &str) {
        let ig = Path::new(&std::env::var("HOME").unwrap()).join(".infigraph");
        std::fs::create_dir_all(&ig).unwrap();
        std::fs::write(ig.join("config.toml"), body).unwrap();
    }

    #[test]
    fn no_project_is_trusted_by_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(trusted_projects_env_name());
        let _home = PinnedHome::empty();
        let project = tempfile::tempdir().unwrap();
        assert!(!is_trusted_project(project.path()));
    }

    #[test]
    fn a_project_listed_in_the_user_config_is_trusted() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(trusted_projects_env_name());
        let _home = PinnedHome::empty();
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        // Listed through a path that is not already canonical.
        let listed = project.path().join(".");
        write_user_config(&format!(
            "[pipelines]\ntrusted_projects = [{:?}]\n",
            listed.to_string_lossy()
        ));
        assert!(is_trusted_project(project.path()));
        assert!(!is_trusted_project(other.path()));
    }

    #[test]
    fn a_project_cannot_trust_itself() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(trusted_projects_env_name());
        let _home = PinnedHome::empty();
        let project = tempfile::tempdir().unwrap();
        let ig = project.path().join(".infigraph");
        std::fs::create_dir_all(&ig).unwrap();
        std::fs::write(
            ig.join("config.toml"),
            format!(
                "[pipelines]\ntrusted_projects = [{:?}]\n",
                project.path().to_string_lossy()
            ),
        )
        .unwrap();
        assert!(!is_trusted_project(project.path()));
    }

    #[test]
    fn a_relative_entry_trusts_nothing() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(trusted_projects_env_name());
        let _home = PinnedHome::empty();
        let project = tempfile::tempdir().unwrap();
        write_user_config("[pipelines]\ntrusted_projects = [\".\", \"\"]\n");
        // Whatever the working directory is, a relative entry vouches for no one.
        assert!(!is_trusted_project(project.path()));
        assert!(!is_trusted_project(&std::env::current_dir().unwrap()));
    }

    #[test]
    fn the_env_override_lists_trusted_projects_comma_separated() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let c = tempfile::tempdir().unwrap();
        std::env::set_var(
            trusted_projects_env_name(),
            format!("{}, {}", a.path().display(), b.path().display()),
        );
        let (ta, tb, tc) = (
            is_trusted_project(a.path()),
            is_trusted_project(b.path()),
            is_trusted_project(c.path()),
        );
        std::env::remove_var(trusted_projects_env_name());
        assert!(ta && tb && !tc);
    }

    #[test]
    fn the_env_name_comes_from_the_definition() {
        assert_eq!(
            trusted_projects_env_name(),
            "INFIGRAPH_PIPELINES_TRUSTED_PROJECTS"
        );
    }
}
