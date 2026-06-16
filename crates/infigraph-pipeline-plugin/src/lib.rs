pub mod config;
pub mod driver;

pub use config::{generate_ddl, ColumnDef, DependencyFields, PipelinePluginConfig, PluginMeta};
pub use driver::{PipelineCoreFields, PipelineData, PipelinePluginDriver, PipelinePluginRegistry};

use std::path::Path;

use anyhow::Result;

/// Discover and load pipeline plugins from ~/.infigraph/pipelines/ and project dir.
pub fn load_pipeline_plugins(
    project_pipelines_dir: Option<&Path>,
) -> Result<PipelinePluginRegistry> {
    let mut registry = PipelinePluginRegistry::new();

    // Scan home dir
    if let Some(home) = dirs_next::home_dir() {
        let global_dir = home.join(".infigraph").join("pipelines");
        if global_dir.is_dir() {
            discover_and_register(&global_dir, &mut registry)?;
        }
    }

    // Scan project dir
    if let Some(dir) = project_pipelines_dir {
        if dir.is_dir() {
            discover_and_register(dir, &mut registry)?;
        }
    }

    Ok(registry)
}

fn discover_and_register(dir: &Path, registry: &mut PipelinePluginRegistry) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!("Failed to read pipeline plugins directory {:?}: {}", dir, e);
            return Ok(());
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
            log::warn!(
                "Invalid pipeline plugin config in {:?}: {}",
                plugin_toml,
                e
            );
            continue;
        }

        log::info!(
            "Discovered pipeline plugin '{}' ({})",
            config.plugin.name,
            config.plugin.plugin_id
        );

        let driver = PipelinePluginDriver::new(config, path);
        registry.register(driver);
    }

    Ok(())
}
