use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::PipelinePluginConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineCoreFields {
    pub name: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineData {
    pub core: PipelineCoreFields,
    #[serde(default)]
    pub properties: serde_json::Map<String, serde_json::Value>,
}

struct DriverProcess {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

pub struct PipelinePluginDriver {
    config: PipelinePluginConfig,
    plugin_dir: PathBuf,
    process: Mutex<Option<DriverProcess>>,
}

impl PipelinePluginDriver {
    /// Create a new driver. Does NOT start the subprocess yet.
    pub fn new(config: PipelinePluginConfig, plugin_dir: PathBuf) -> Self {
        Self {
            config,
            plugin_dir,
            process: Mutex::new(None),
        }
    }

    /// Spawn the subprocess and wait for the ready handshake.
    pub fn start(&self) -> Result<()> {
        let cmd = &self.config.plugin.command;
        if cmd.is_empty() {
            bail!("Plugin command is empty");
        }

        let mut child = Command::new(&cmd[0])
            .args(&cmd[1..])
            .current_dir(&self.plugin_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| {
                format!(
                    "Failed to spawn plugin '{}': {:?}",
                    self.config.plugin.plugin_id, cmd
                )
            })?;

        let stdout = child
            .stdout
            .take()
            .context("Failed to capture plugin stdout")?;
        let mut reader = BufReader::new(stdout);

        // Read handshake line
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .context("Failed to read ready handshake from plugin")?;

        let handshake: serde_json::Value = serde_json::from_str(line.trim())
            .with_context(|| format!("Invalid handshake JSON: {}", line.trim()))?;

        if handshake.get("ready") != Some(&serde_json::Value::Bool(true)) {
            bail!(
                "Plugin '{}' did not send {{\"ready\": true}}, got: {}",
                self.config.plugin.plugin_id,
                line.trim()
            );
        }

        let mut proc = self.process.lock().unwrap();
        *proc = Some(DriverProcess { child, reader });
        Ok(())
    }

    /// Send an extract command and read the response.
    pub fn extract(
        &self,
        content: &str,
        title: &str,
        doc_id: &str,
    ) -> Result<Option<PipelineData>> {
        let mut proc_guard = self.process.lock().unwrap();
        let proc = proc_guard
            .as_mut()
            .context("Plugin process not started — call start() first")?;

        let request = serde_json::json!({
            "command": "extract",
            "content": content,
            "title": title,
            "doc_id": doc_id,
        });

        let stdin = proc
            .child
            .stdin
            .as_mut()
            .context("Plugin stdin unavailable")?;
        let mut request_bytes = serde_json::to_vec(&request)?;
        request_bytes.push(b'\n');
        stdin
            .write_all(&request_bytes)
            .context("Failed to write to plugin stdin")?;
        stdin.flush().context("Failed to flush plugin stdin")?;

        let mut line = String::new();
        proc.reader
            .read_line(&mut line)
            .context("Failed to read plugin response")?;

        let response: serde_json::Value = serde_json::from_str(line.trim())
            .with_context(|| format!("Invalid response JSON: {}", line.trim()))?;

        match response.get("status").and_then(|v| v.as_str()) {
            Some("ok") => {
                let data: PipelineData = serde_json::from_value(
                    response
                        .get("data")
                        .cloned()
                        .context("Missing 'data' field in ok response")?,
                )
                .context("Failed to deserialize PipelineData")?;
                Ok(Some(data))
            }
            Some("skip") => Ok(None),
            Some("error") => {
                let msg = response
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                bail!("Plugin '{}' returned error: {}", self.config.plugin.plugin_id, msg);
            }
            other => {
                bail!(
                    "Plugin '{}' returned unexpected status: {:?}",
                    self.config.plugin.plugin_id,
                    other
                );
            }
        }
    }

    /// Get the plugin configuration.
    pub fn config(&self) -> &PipelinePluginConfig {
        &self.config
    }

    /// Get the plugin ID.
    pub fn plugin_id(&self) -> &str {
        &self.config.plugin.plugin_id
    }
}

impl Drop for PipelinePluginDriver {
    fn drop(&mut self) {
        if let Ok(mut proc) = self.process.lock() {
            if let Some(ref mut dp) = *proc {
                let _ = dp.child.kill();
                let _ = dp.child.wait();
            }
        }
    }
}

pub struct PipelinePluginRegistry {
    plugins: Vec<PipelinePluginDriver>,
}

impl PipelinePluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    pub fn register(&mut self, driver: PipelinePluginDriver) {
        self.plugins.push(driver);
    }

    /// Extract using a specific plugin by ID.
    pub fn extract(
        &self,
        plugin_id: &str,
        content: &str,
        title: &str,
        doc_id: &str,
    ) -> Result<Option<PipelineData>> {
        let driver = self
            .get_plugin(plugin_id)
            .with_context(|| format!("No plugin found with id '{}'", plugin_id))?;
        driver.extract(content, title, doc_id)
    }

    /// Try each plugin's detect_patterns against content; first match wins.
    pub fn extract_auto(
        &self,
        content: &str,
        title: &str,
        doc_id: &str,
    ) -> Result<Option<(String, PipelineData)>> {
        for driver in &self.plugins {
            for pattern in &driver.config().plugin.detect_patterns {
                match Regex::new(pattern) {
                    Ok(re) => {
                        if re.is_match(content) {
                            let result = driver.extract(content, title, doc_id)?;
                            if let Some(data) = result {
                                return Ok(Some((driver.plugin_id().to_string(), data)));
                            }
                            // Plugin matched pattern but returned skip — continue to next plugin
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Invalid detect_pattern '{}' in plugin '{}': {}",
                            pattern,
                            driver.plugin_id(),
                            e
                        );
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn plugins(&self) -> &[PipelinePluginDriver] {
        &self.plugins
    }

    pub fn get_plugin(&self, plugin_id: &str) -> Option<&PipelinePluginDriver> {
        self.plugins.iter().find(|p| p.plugin_id() == plugin_id)
    }

    pub fn plugin_ids(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.plugin_id()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

impl Default for PipelinePluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if any of the given patterns match the content.
/// Exposed for testing without subprocess.
pub fn matches_detect_patterns(content: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        if let Ok(re) = Regex::new(pattern) {
            if re.is_match(content) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipeline_data_deserialize() {
        let json = r#"{
            "core": {
                "name": "build_step",
                "inputs": ["source.tar"],
                "outputs": ["binary"]
            },
            "properties": {
                "stage_name": "compile",
                "order": 1
            }
        }"#;
        let data: PipelineData = serde_json::from_str(json).unwrap();
        assert_eq!(data.core.name, "build_step");
        assert_eq!(data.core.inputs, vec!["source.tar"]);
        assert_eq!(data.core.outputs, vec!["binary"]);
        assert_eq!(
            data.properties.get("stage_name").unwrap(),
            &serde_json::Value::String("compile".to_string())
        );
        assert_eq!(
            data.properties.get("order").unwrap(),
            &serde_json::json!(1)
        );
    }

    #[test]
    fn test_pipeline_data_skip() {
        // A skip response has status=skip and no data
        let json = r#"{"status": "skip"}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v.get("status").unwrap().as_str().unwrap(), "skip");
    }

    #[test]
    fn test_registry_empty() {
        let registry = PipelinePluginRegistry::new();
        assert!(registry.is_empty());
        assert!(registry.plugin_ids().is_empty());
    }

    #[test]
    fn test_detect_patterns() {
        let patterns = vec![
            r"stage\s*\(".to_string(),
            r"pipeline\s*\{".to_string(),
        ];

        assert!(matches_detect_patterns("  stage ( foo )", &patterns));
        assert!(matches_detect_patterns("pipeline {", &patterns));
        assert!(!matches_detect_patterns("no match here", &patterns));
        assert!(!matches_detect_patterns("", &patterns));
    }
}
