//! Pipeline plugins during document indexing.
//!
//! A [`PipelineRun`] lives for one indexing run. It loads the plugin registry
//! once (`infigraph_pipeline_plugin::load_pipeline_plugins` decides which
//! plugins may run), starts a plugin lazily on the first document that
//! matches its `detect_patterns`, writes what a plugin extracts through the
//! `DocBackend` pipeline methods, and ends when it is dropped: the registry
//! goes with it and every plugin's process group is killed.
//!
//! It is called right after `upsert_docs`, inside the same call that holds
//! `docs-op.lock` exclusively, so it takes no lock of its own.
//!
//! Nothing here can fail the indexing run. A plugin that errors, times out or
//! will not start costs a warning, and the run goes on. For a document that
//! plugin was the first match for, the document's existing rows are kept, not
//! deleted: "the plugin failed" is not "the document has no pipeline".

use std::collections::HashSet;
use std::path::Path;

use infigraph_core::child::ChildTimeouts;
use infigraph_pipeline_plugin::{load_pipeline_plugins, PipelineData, PipelinePluginRegistry};
use regex::Regex;

use crate::backend::DocBackend;
use crate::extract::ExtractedDoc;
use crate::store::PipelineCoreRecord;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not started yet; starts on its first matching document.
    Idle,
    Running,
    /// Failed to start or was poisoned: skipped for the rest of the run.
    Dead,
}

/// What the first plugin that matched a document made of it.
enum Outcome {
    /// A pipeline, from plugin `index` in the registry.
    Pipeline(usize, PipelineData),
    /// No plugin matched, or every matching plugin skipped it.
    None,
    /// A matching plugin failed: keep what the document had.
    Failed,
}

pub struct PipelineRun {
    registry: PipelinePluginRegistry,
    timeouts: ChildTimeouts,
    state: Vec<State>,
    /// Each plugin's compiled `detect_patterns`, built on first use.
    patterns: Vec<Option<Vec<Regex>>>,
    /// Plugins whose `Pipeline_<id>` table this run has already ensured.
    tables_ready: HashSet<String>,
    /// Pipeline ids already reported as named by two documents.
    collided: HashSet<String>,
    warnings: Vec<String>,
    touched: bool,
}

impl PipelineRun {
    /// A run for the project at `root`. A plugin registry that cannot be
    /// loaded is a warning and an empty run. `timeouts` is a test seam;
    /// production passes `ChildTimeouts::DEFAULT`.
    pub fn for_project(root: &Path, timeouts: ChildTimeouts) -> Self {
        let mut warnings = Vec::new();
        let registry = match load_pipeline_plugins(Some(root)) {
            Ok(registry) => registry,
            Err(e) => {
                warn(
                    &mut warnings,
                    format!("could not load pipeline plugins: {e:#}"),
                );
                PipelinePluginRegistry::new()
            }
        };
        let n = registry.plugins().len();
        Self {
            registry,
            timeouts,
            state: vec![State::Idle; n],
            patterns: (0..n).map(|_| None).collect(),
            tables_ready: HashSet::new(),
            collided: HashSet::new(),
            warnings,
            touched: false,
        }
    }

    /// Offers the documents just written by `upsert_docs` to the plugins, and
    /// stores what they extract. Never fails the caller.
    pub fn apply(&mut self, store: &dyn DocBackend, docs: &[&ExtractedDoc]) {
        if self.registry.is_empty() || docs.is_empty() {
            return;
        }
        let _phase = infigraph_core::write_phase::enter(&"docs: pipeline rows", docs.len() as u64);
        self.touched = true;

        let mut cleared: Vec<&str> = Vec::new();
        let mut kept: Vec<&str> = Vec::new();
        for doc in docs {
            match self.extract_for(doc) {
                Outcome::Pipeline(idx, data) => {
                    if !self.replace(store, idx, doc, data) {
                        kept.push(&doc.file);
                    }
                }
                Outcome::None => cleared.push(&doc.file),
                Outcome::Failed => kept.push(&doc.file),
            }
        }

        if let Err(e) = store.delete_pipelines_for_docs(&cleared) {
            self.warn(format!("could not delete pipeline rows: {e:#}"));
        }
        self.relink(store, &kept);
    }

    /// Everything this run warned about so far.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Ends the run: rebuilds the dependency edges once if anything was
    /// offered to plugins, then drops the registry, which kills the plugin
    /// processes. Returns the run's warnings.
    pub fn finish(mut self, store: &dyn DocBackend) -> Vec<String> {
        if self.touched {
            if let Err(e) = store.link_pipeline_dependencies() {
                self.warn(format!("could not link pipeline dependencies: {e:#}"));
            }
        }
        self.warnings
    }

    fn warn(&mut self, message: String) {
        warn(&mut self.warnings, message);
    }

    /// The first plugin (in plugin-id order) whose patterns match the
    /// document decides it.
    fn extract_for(&mut self, doc: &ExtractedDoc) -> Outcome {
        for idx in 0..self.registry.plugins().len() {
            if !self.matches(idx, &doc.text) {
                continue;
            }
            let driver = &self.registry.plugins()[idx];
            let id = driver.plugin_id().to_string();
            if self.state[idx] == State::Dead {
                return Outcome::Failed;
            }
            if self.state[idx] == State::Idle {
                match driver.start_with(self.timeouts) {
                    Ok(()) => self.state[idx] = State::Running,
                    Err(e) => {
                        self.state[idx] = State::Dead;
                        warn(
                            &mut self.warnings,
                            format!("pipeline plugin '{id}' did not start: {e:#}"),
                        );
                        return Outcome::Failed;
                    }
                }
            }
            let title = doc.title.clone().unwrap_or_else(|| doc.file.clone());
            match driver.extract(&doc.text, &title, &doc.file) {
                Ok(Some(data)) => return Outcome::Pipeline(idx, data),
                // This plugin has nothing for the document: the next may.
                Ok(None) => continue,
                Err(e) => {
                    if driver.is_poisoned() {
                        self.state[idx] = State::Dead;
                        warn(
                            &mut self.warnings,
                            format!(
                                "pipeline plugin '{id}' stopped (skipped for the rest of this \
                                 run): {e:#}"
                            ),
                        );
                    } else {
                        warn(
                            &mut self.warnings,
                            format!("pipeline plugin '{id}' failed on '{}': {e:#}", doc.file),
                        );
                    }
                    return Outcome::Failed;
                }
            }
        }
        Outcome::None
    }

    /// Whether plugin `idx`'s `detect_patterns` match `text`. An invalid
    /// pattern is warned about once and ignored; a plugin with no valid
    /// pattern matches nothing.
    fn matches(&mut self, idx: usize, text: &str) -> bool {
        if self.patterns[idx].is_none() {
            let driver = &self.registry.plugins()[idx];
            let mut compiled = Vec::new();
            for pattern in &driver.config().plugin.detect_patterns {
                match Regex::new(pattern) {
                    Ok(re) => compiled.push(re),
                    Err(e) => warn(
                        &mut self.warnings,
                        format!(
                            "pipeline plugin '{}': invalid detect_pattern '{pattern}': {e}",
                            driver.plugin_id()
                        ),
                    ),
                }
            }
            self.patterns[idx] = Some(compiled);
        }
        self.patterns[idx]
            .as_ref()
            .is_some_and(|patterns| patterns.iter().any(|re| re.is_match(text)))
    }

    /// Replaces the document's pipeline rows with the extracted one. Returns
    /// false when that failed. If the plugin's table could not be ensured the
    /// old rows are untouched; if a later write failed the document's pipeline
    /// rows have been removed (a half-written pipeline is worse) and are
    /// rebuilt on its next change or by `infigraph reindex-docs`.
    fn replace(
        &mut self,
        store: &dyn DocBackend,
        idx: usize,
        doc: &ExtractedDoc,
        data: PipelineData,
    ) -> bool {
        let cfg = &self.registry.plugins()[idx].config().plugin;
        let plugin_id = cfg.plugin_id.clone();
        let columns: Vec<(String, String)> = cfg
            .schema
            .iter()
            .map(|c| (c.name.clone(), c.col_type.clone()))
            .collect();

        if !self.tables_ready.contains(&plugin_id) {
            if let Err(e) = store.ensure_plugin_table(&plugin_id, &columns) {
                self.warn(format!("pipeline plugin '{plugin_id}': {e:#}"));
                return false;
            }
            self.tables_ready.insert(plugin_id.clone());
        }

        let id = format!("pipeline::{plugin_id}::{}", data.core.name);
        let record = PipelineCoreRecord {
            id: id.clone(),
            name: data.core.name.clone(),
            doc_id: doc.file.clone(),
            plugin_id: plugin_id.clone(),
            inputs: data.core.inputs.clone(),
            outputs: data.core.outputs.clone(),
        };
        self.warn_if_taken_by_another_document(store, &id, &doc.file);
        let written = store
            .delete_pipelines_for_docs(&[doc.file.as_str()])
            .and_then(|()| store.upsert_pipeline_core(&record))
            .and_then(|()| {
                store.upsert_plugin_properties(&id, &plugin_id, &data.properties, &columns)
            })
            .and_then(|()| store.link_pipeline_core_to_doc(&id, &doc.file));
        match written {
            Ok(()) => true,
            Err(e) => {
                self.warn(format!(
                    "pipeline plugin '{plugin_id}': could not store '{}': {e:#}; its pipeline \
                     rows were removed and are rebuilt when it next changes or on \
                     `infigraph reindex-docs`",
                    doc.file
                ));
                // Do not leave a half-written pipeline behind.
                let _ = store.delete_pipelines_for_docs(&[doc.file.as_str()]);
                false
            }
        }
    }

    /// The id is `pipeline::<plugin_id>::<name>`, so a second document naming
    /// the same pipeline takes over the first one's core. The id form is
    /// fixed; say so, once per id per run, naming both documents.
    fn warn_if_taken_by_another_document(&mut self, store: &dyn DocBackend, id: &str, doc: &str) {
        if self.collided.contains(id) {
            return;
        }
        if let Ok(Some(existing)) = store.get_pipeline_core(id) {
            if existing.doc_id != doc {
                self.collided.insert(id.to_string());
                self.warn(format!(
                    "pipeline '{id}' is named by both '{}' and '{doc}'; '{doc}' now owns it",
                    existing.doc_id
                ));
            }
        }
    }

    /// `upsert_docs` replaces a changed document's node, which drops its
    /// edges. A pipeline kept because its plugin failed must be linked to the
    /// new node again; `link_pipeline_core_to_doc` does not duplicate an edge.
    fn relink(&mut self, store: &dyn DocBackend, kept: &[&str]) {
        if kept.is_empty() {
            return;
        }
        let cores = match store.get_all_pipeline_cores(None) {
            Ok(cores) => cores,
            Err(e) => {
                self.warn(format!("could not read pipelines to relink: {e:#}"));
                return;
            }
        };
        for core in cores.iter().filter(|c| kept.contains(&c.doc_id.as_str())) {
            if let Err(e) = store.link_pipeline_core_to_doc(&core.id, &core.doc_id) {
                self.warn(format!("could not relink pipeline '{}': {e:#}", core.id));
            }
        }
    }
}

fn warn(warnings: &mut Vec<String>, message: String) {
    eprintln!("[pipelines] {message}");
    warnings.push(message);
}
