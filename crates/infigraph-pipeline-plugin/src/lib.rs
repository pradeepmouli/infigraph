pub mod config;
pub mod driver;

pub use config::{generate_ddl, ColumnDef, DependencyFields, PipelinePluginConfig, PluginMeta};
pub use driver::{PipelineCoreFields, PipelineData, PipelinePluginDriver, PipelinePluginRegistry};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// The plugins that may run for a project, and the one function that decides.
///
/// User-level plugins (`~/.infigraph/pipelines`) always load: they belong to
/// the person running infigraph. Project-level plugins (`<root>/pipelines`)
/// arrive with the repository, so they load only when the user layer lists
/// the project in `[pipelines] trusted_projects` (see
/// `infigraph_core::pipelines_trust`). A plugin_id present in both layers is
/// the project's. The result is ordered by plugin_id, so which plugin wins a
/// document does not depend on directory-listing order.
pub fn load_pipeline_plugins(project_root: Option<&Path>) -> Result<PipelinePluginRegistry> {
    let mut by_id: BTreeMap<String, PipelinePluginDriver> = BTreeMap::new();

    if let Some(home) = dirs_next::home_dir() {
        let global_dir = home.join(".infigraph").join("pipelines");
        if global_dir.is_dir() {
            for driver in discover(&global_dir)? {
                by_id.insert(driver.plugin_id().to_string(), driver);
            }
        }
    }

    if let Some(root) = project_root {
        let dir = root.join("pipelines");
        if dir.is_dir() && infigraph_core::pipelines_trust::is_trusted_project(root) {
            for driver in discover(&dir)? {
                by_id.insert(driver.plugin_id().to_string(), driver);
            }
        }
    }

    let mut registry = PipelinePluginRegistry::new();
    for driver in by_id.into_values() {
        registry.register(driver);
    }
    Ok(registry)
}

/// The project's `pipelines` directory when it exists but its plugins are
/// not allowed to run, so a listing can say so.
pub fn untrusted_project_pipelines(project_root: &Path) -> Option<PathBuf> {
    let dir = project_root.join("pipelines");
    (dir.is_dir() && !infigraph_core::pipelines_trust::is_trusted_project(project_root))
        .then_some(dir)
}

/// One line telling a listing's reader why a project's own plugins are
/// absent, and how to allow them; `None` when nothing was skipped.
pub fn untrusted_project_note(project_root: &Path) -> Option<String> {
    let dir = untrusted_project_pipelines(project_root)?;
    Some(format!(
        "Not loaded: {} -- project-level pipeline plugins run only for a project listed in \
         `[pipelines] trusted_projects` of ~/.infigraph/config.toml (or {}).",
        dir.display(),
        infigraph_core::pipelines_trust::trusted_projects_env_name()
    ))
}

/// Every valid plugin under `dir`, in no particular order.
fn discover(dir: &Path) -> Result<Vec<PipelinePluginDriver>> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!("Failed to read pipeline plugins directory {:?}: {}", dir, e);
            return Ok(found);
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log::warn!("Failed to read directory entry in {:?}: {}", dir, e);
                continue;
            }
        };

        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let plugin_toml = path.join("plugin.toml");
        if !plugin_toml.is_file() {
            continue;
        }

        let toml_content = match std::fs::read_to_string(&plugin_toml) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Failed to read {:?}: {}", plugin_toml, e);
                continue;
            }
        };

        let config: PipelinePluginConfig = match toml::from_str(&toml_content) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("Failed to parse {:?}: {}", plugin_toml, e);
                continue;
            }
        };

        if let Err(e) = config.plugin.validate() {
            log::warn!("Invalid pipeline plugin config in {:?}: {}", plugin_toml, e);
            continue;
        }

        log::info!(
            "Discovered pipeline plugin '{}' ({})",
            config.plugin.name,
            config.plugin.plugin_id
        );

        found.push(PipelinePluginDriver::new(config, path));
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HOME` and `INFIGRAPH_PIPELINES_TRUSTED_PROJECTS` are process-global.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Pins `$HOME` at an empty directory (no user-level plugins, no user
    /// config) and clears the trust env var; restores both on drop.
    struct Pinned {
        home: tempfile::TempDir,
        orig_home: Option<String>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Pinned {
        fn new() -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let home = tempfile::tempdir().unwrap();
            let orig_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", home.path());
            std::env::remove_var(infigraph_core::pipelines_trust::trusted_projects_env_name());
            Self {
                home,
                orig_home,
                _guard: guard,
            }
        }

        fn user_pipelines(&self) -> std::path::PathBuf {
            self.home.path().join(".infigraph").join("pipelines")
        }

        fn trust(&self, root: &Path) {
            let ig = self.home.path().join(".infigraph");
            std::fs::create_dir_all(&ig).unwrap();
            std::fs::write(
                ig.join("config.toml"),
                format!(
                    "[pipelines]\ntrusted_projects = [{:?}]\n",
                    root.to_string_lossy()
                ),
            )
            .unwrap();
        }
    }

    impl Drop for Pinned {
        fn drop(&mut self) {
            match &self.orig_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn write_plugin(pipelines_dir: &Path, id: &str, name: &str) {
        let dir = pipelines_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            format!("[plugin]\nname = \"{name}\"\nplugin_id = \"{id}\"\ncommand = [\"true\"]\n"),
        )
        .unwrap();
    }

    fn ids(registry: &PipelinePluginRegistry) -> Vec<String> {
        registry
            .plugin_ids()
            .into_iter()
            .map(String::from)
            .collect()
    }

    #[test]
    fn test_load_pipeline_plugins_empty() {
        let pinned = Pinned::new();
        let dir = pinned.home.path().join("some-project");
        std::fs::create_dir_all(&dir).unwrap();
        let registry = load_pipeline_plugins(Some(&dir)).unwrap();
        assert!(registry.get_plugin("nonexistent").is_none());
    }

    /// A cloned repo must not get its commands run: a project-level plugin
    /// loads only for a project the user layer trusts.
    #[test]
    fn an_untrusted_projects_plugins_are_not_loaded() {
        let pinned = Pinned::new();
        let project = tempfile::tempdir().unwrap();
        write_plugin(&project.path().join("pipelines"), "theirs", "Theirs");
        let registry = load_pipeline_plugins(Some(project.path())).unwrap();
        assert!(registry.is_empty(), "loaded {:?}", ids(&registry));
        assert_eq!(
            untrusted_project_pipelines(project.path()),
            Some(project.path().join("pipelines"))
        );
        let note = untrusted_project_note(project.path()).expect("a note for the skipped dir");
        assert!(note.contains("trusted_projects"), "{note}");
        assert!(
            note.contains("INFIGRAPH_PIPELINES_TRUSTED_PROJECTS"),
            "{note}"
        );
        drop(pinned);
    }

    #[test]
    fn a_trusted_projects_plugins_are_loaded() {
        let pinned = Pinned::new();
        let project = tempfile::tempdir().unwrap();
        write_plugin(&project.path().join("pipelines"), "theirs", "Theirs");
        pinned.trust(project.path());
        let registry = load_pipeline_plugins(Some(project.path())).unwrap();
        assert_eq!(ids(&registry), vec!["theirs"]);
        assert_eq!(untrusted_project_pipelines(project.path()), None);
        assert_eq!(untrusted_project_note(project.path()), None);
    }

    #[test]
    fn a_project_cannot_enable_its_own_plugins_through_its_config() {
        let pinned = Pinned::new();
        let project = tempfile::tempdir().unwrap();
        write_plugin(&project.path().join("pipelines"), "theirs", "Theirs");
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
        let registry = load_pipeline_plugins(Some(project.path())).unwrap();
        assert!(registry.is_empty(), "loaded {:?}", ids(&registry));
        drop(pinned);
    }

    /// The docs say the project-level plugin wins; the code used to keep both
    /// and answer with whichever was registered first.
    #[test]
    fn a_project_plugin_overrides_a_user_plugin_with_the_same_id() {
        let pinned = Pinned::new();
        let project = tempfile::tempdir().unwrap();
        write_plugin(&pinned.user_pipelines(), "shared", "User version");
        write_plugin(
            &project.path().join("pipelines"),
            "shared",
            "Project version",
        );
        pinned.trust(project.path());
        let registry = load_pipeline_plugins(Some(project.path())).unwrap();
        assert_eq!(ids(&registry), vec!["shared"]);
        assert_eq!(
            registry.get_plugin("shared").unwrap().config().plugin.name,
            "Project version"
        );
    }

    #[test]
    fn plugins_load_in_plugin_id_order() {
        let pinned = Pinned::new();
        for id in ["zeta", "alpha", "mid"] {
            write_plugin(&pinned.user_pipelines(), id, id);
        }
        let registry = load_pipeline_plugins(None).unwrap();
        assert_eq!(ids(&registry), vec!["alpha", "mid", "zeta"]);
    }
}
