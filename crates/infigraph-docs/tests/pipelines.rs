//! `PipelineRun`: what a run does with the documents it is handed, against a
//! real `DocStore` and fixture plugins that are `sh` scripts speaking the line
//! protocol.

#![cfg(unix)]

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use infigraph_core::child::ChildTimeouts;
use infigraph_docs::extract::{DocFormat, ExtractedDoc};
use infigraph_docs::pipelines::PipelineRun;
use infigraph_docs::store::DocStore;
use infigraph_docs::DocIndex;

/// `HOME` and the trust variable are process-global.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const QUICK: ChildTimeouts = ChildTimeouts {
    ready: Duration::from_secs(10),
    request: Duration::from_millis(600),
};

/// A user layer with no plugins and no trusted projects, restored on drop.
struct Env {
    home: tempfile::TempDir,
    orig_home: Option<String>,
    _guard: MutexGuard<'static, ()>,
}

impl Env {
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

    /// A user-level plugin `id` whose command is `script` run by `sh`.
    fn plugin(&self, id: &str, patterns: &[&str], script: &str) {
        self.plugin_command(id, patterns, "[\"sh\", \"extract.sh\"]");
        std::fs::write(
            self.home
                .path()
                .join(".infigraph/pipelines")
                .join(id)
                .join("extract.sh"),
            script,
        )
        .unwrap();
    }

    /// A user-level plugin `id` with `command` (a TOML array) as given.
    fn plugin_command(&self, id: &str, patterns: &[&str], command: &str) {
        let dir = self.home.path().join(".infigraph/pipelines").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let patterns: Vec<String> = patterns.iter().map(|p| format!("{p:?}")).collect();
        std::fs::write(
            dir.join("plugin.toml"),
            format!(
                "[plugin]\nname = \"{id}\"\nplugin_id = \"{id}\"\ncommand = {command}\n\
                 detect_patterns = [{}]\n\n[[plugin.schema]]\nname = \"owner\"\ncol_type = \"STRING\"\n",
                patterns.join(", ")
            ),
        )
        .unwrap();
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        match &self.orig_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }
}

/// Answers by what the document starts with: `SKIP` skips, `ERROR` reports an
/// error, `HANG` never answers, anything else is a pipeline named by its
/// `name=<word>`.
const SCRIPT: &str = r#"#!/bin/sh
echo '{"ready":true}'
while IFS= read -r line; do
  case "$line" in
    *'"content":"SKIP'*) echo '{"status":"skip"}';;
    *'"content":"ERROR'*) echo '{"status":"error","message":"boom"}';;
    *'"content":"HANG'*) sleep 600;;
    *) name=$(printf '%s' "$line" | sed 's/.*name=\([a-z0-9_]*\).*/\1/')
       echo "{\"status\":\"ok\",\"data\":{\"core\":{\"name\":\"$name\",\"inputs\":[\"in_$name\"],\"outputs\":[\"out_$name\"]},\"properties\":{\"owner\":\"o_$name\"}}}";;
  esac
done
"#;

const PATTERNS: &[&str] = &["name=", "SKIP", "ERROR", "HANG"];

fn doc(file: &str, text: &str) -> ExtractedDoc {
    ExtractedDoc {
        file: file.to_string(),
        title: None,
        content_hash: "h".to_string(),
        format: DocFormat::Markdown,
        text: text.to_string(),
        page_count: None,
    }
}

fn store() -> (tempfile::TempDir, DocStore) {
    let tmp = tempfile::tempdir().unwrap();
    let store = DocStore::open(&tmp.path().join("docs.kuzu")).unwrap();
    (tmp, store)
}

/// What `upsert_docs` does to a changed document: its node is replaced, which
/// drops the node's edges.
fn replace_document(store: &DocStore, file: &str) {
    let conn = store.connection().unwrap();
    let _ = conn.query(&format!(
        "MATCH (d:Document {{id: '{file}'}}) DETACH DELETE d"
    ));
    conn.query(&format!(
        "CREATE (d:Document {{id: '{file}', file: '{file}', title: 't', content_hash: 'h'}})"
    ))
    .unwrap();
}

/// One indexing step: the documents are (re)written, then offered to the run.
fn index(run: &mut PipelineRun, store: &DocStore, docs: &[(&str, &str)]) {
    for (file, _) in docs {
        replace_document(store, file);
    }
    let docs: Vec<ExtractedDoc> = docs.iter().map(|(f, t)| doc(f, t)).collect();
    let refs: Vec<&ExtractedDoc> = docs.iter().collect();
    run.apply(store, &refs);
}

fn core_ids(store: &DocStore) -> Vec<String> {
    let mut ids: Vec<String> = store
        .get_all_pipeline_cores(None)
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    ids.sort();
    ids
}

fn defined_in(store: &DocStore, core_id: &str) -> Vec<String> {
    store
        .connection()
        .unwrap()
        .query(&format!(
            "MATCH (p:PipelineCore)-[:DEFINED_IN]->(d:Document) WHERE p.id = '{core_id}' RETURN d.id"
        ))
        .unwrap()
        .map(|r| r[0].to_string())
        .collect()
}

fn plugin_rows(store: &DocStore, plugin: &str, needle: &str) -> usize {
    store
        .query_plugin_table(plugin, "owner", needle)
        .map(|r| r.len())
        .unwrap_or(0)
}

fn run_for(env: &Env) -> PipelineRun {
    // No project-level pipelines directory, so only the user layer loads.
    PipelineRun::for_project(env.home.path().join("no-such-project").as_path(), QUICK)
}

#[test]
fn an_extracted_pipeline_is_stored_with_its_properties_and_its_document() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    let (_tmp, store) = store();
    let mut run = run_for(&env);

    index(&mut run, &store, &[("a.md", "name=alpha\nsome text")]);
    run.finish(&store);

    assert_eq!(core_ids(&store), vec!["pipeline::fake::alpha"]);
    let core = store
        .get_pipeline_core("pipeline::fake::alpha")
        .unwrap()
        .unwrap();
    assert_eq!(core.doc_id, "a.md");
    assert_eq!(core.plugin_id, "fake");
    assert_eq!(core.inputs, vec!["in_alpha"]);
    assert_eq!(core.outputs, vec!["out_alpha"]);
    assert_eq!(plugin_rows(&store, "fake", "o_alpha"), 1);
    assert_eq!(defined_in(&store, "pipeline::fake::alpha"), vec!["a.md"]);
}

#[test]
fn a_skip_or_a_document_no_plugin_matches_deletes_the_documents_old_rows() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    let (_tmp, store) = store();
    let mut run = run_for(&env);

    index(
        &mut run,
        &store,
        &[("a.md", "name=alpha"), ("b.md", "name=beta")],
    );
    assert_eq!(core_ids(&store).len(), 2);

    // a.md now says SKIP; b.md matches nothing any more.
    index(
        &mut run,
        &store,
        &[("a.md", "SKIP this one"), ("b.md", "plain prose")],
    );
    assert!(core_ids(&store).is_empty(), "{:?}", core_ids(&store));
    assert_eq!(plugin_rows(&store, "fake", "o_"), 0);
}

#[test]
fn an_edited_document_that_renames_its_pipeline_leaves_no_row_for_the_old_name() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    let (_tmp, store) = store();
    let mut run = run_for(&env);

    index(&mut run, &store, &[("a.md", "name=alpha")]);
    index(&mut run, &store, &[("a.md", "name=beta")]);

    assert_eq!(core_ids(&store), vec!["pipeline::fake::beta"]);
    assert_eq!(plugin_rows(&store, "fake", "o_alpha"), 0);
    assert_eq!(plugin_rows(&store, "fake", "o_beta"), 1);
}

/// A plugin error is not "this document has no pipeline": the old rows stay,
/// and, because `upsert_docs` replaced the document's node, they are linked to
/// the new one.
#[test]
fn a_plugin_error_keeps_the_documents_rows_and_relinks_them_to_the_new_node() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    let (_tmp, store) = store();
    let mut run = run_for(&env);

    index(&mut run, &store, &[("a.md", "name=alpha")]);
    assert_eq!(defined_in(&store, "pipeline::fake::alpha"), vec!["a.md"]);

    index(&mut run, &store, &[("a.md", "ERROR in this edit")]);
    run.finish(&store);

    assert_eq!(core_ids(&store), vec!["pipeline::fake::alpha"]);
    assert_eq!(plugin_rows(&store, "fake", "o_alpha"), 1);
    assert_eq!(
        defined_in(&store, "pipeline::fake::alpha"),
        vec!["a.md"],
        "the kept pipeline lost its DEFINED_IN edge to the replaced document"
    );
}

#[test]
fn the_first_matching_plugin_wins_and_two_plugins_do_not_overwrite_each_other() {
    let env = Env::new();
    env.plugin("aaa", &["AAA", "shared="], SCRIPT);
    env.plugin("bbb", &["BBB", "shared="], SCRIPT);
    let (_tmp, store) = store();
    let mut run = run_for(&env);

    index(
        &mut run,
        &store,
        &[
            ("one.md", "AAA name=same"),
            ("two.md", "BBB name=same"),
            ("three.md", "shared= name=both"),
        ],
    );

    // Same pipeline name under two plugins: two rows. A document both plugins
    // match belongs to the first (plugins are ordered by id).
    assert_eq!(
        core_ids(&store),
        vec![
            "pipeline::aaa::both",
            "pipeline::aaa::same",
            "pipeline::bbb::same"
        ]
    );
}

/// A hung plugin costs one timeout and one warning, not one per document, and
/// leaves every document's rows alone.
#[test]
fn a_hung_plugin_is_skipped_for_the_rest_of_the_run_with_one_warning() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    let (_tmp, store) = store();
    let mut run = run_for(&env);

    index(&mut run, &store, &[("a.md", "name=alpha")]);
    let started = std::time::Instant::now();
    index(
        &mut run,
        &store,
        &[("a.md", "HANG 1"), ("b.md", "HANG 2"), ("c.md", "HANG 3")],
    );
    let elapsed = started.elapsed();

    assert!(elapsed < Duration::from_secs(5), "waited {elapsed:?}");
    let warnings = run.finish(&store);
    let about_fake = warnings.iter().filter(|w| w.contains("fake")).count();
    assert_eq!(about_fake, 1, "{warnings:?}");
    assert_eq!(core_ids(&store), vec!["pipeline::fake::alpha"]);
}

#[test]
fn a_plugin_that_cannot_start_is_one_warning_and_the_run_carries_on() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    // Make its command a program that does not exist.
    let toml = env
        .home
        .path()
        .join(".infigraph/pipelines/fake/plugin.toml");
    let body = std::fs::read_to_string(&toml)
        .unwrap()
        .replace("[\"sh\", \"extract.sh\"]", "[\"no-such-program-xyz\"]");
    std::fs::write(&toml, body).unwrap();
    let (_tmp, store) = store();
    let mut run = run_for(&env);

    index(
        &mut run,
        &store,
        &[("a.md", "name=alpha"), ("b.md", "name=beta")],
    );
    let warnings = run.finish(&store);

    assert_eq!(
        warnings.iter().filter(|w| w.contains("fake")).count(),
        1,
        "{warnings:?}"
    );
    assert!(core_ids(&store).is_empty());
}

#[test]
fn a_project_with_no_plugins_touches_nothing() {
    let env = Env::new();
    let (_tmp, store) = store();
    store
        .upsert_pipeline_core(&infigraph_docs::store::PipelineCoreRecord {
            id: "pipeline::x::y".into(),
            name: "y".into(),
            doc_id: "a.md".into(),
            plugin_id: "x".into(),
            inputs: vec![],
            outputs: vec![],
        })
        .unwrap();
    let mut run = run_for(&env);
    index(&mut run, &store, &[("a.md", "name=alpha")]);
    assert!(run.finish(&store).is_empty());
    assert_eq!(core_ids(&store), vec!["pipeline::x::y"]);
}

/// `pipeline::<plugin>::<name>` is the id (ruling D3), so two documents that
/// name the same pipeline share a core and the second takes it. That is
/// reported, once per id per run, naming both documents.
#[test]
fn two_documents_naming_the_same_pipeline_warn_once_naming_both() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    let (_tmp, store) = store();
    let mut run = run_for(&env);

    index(&mut run, &store, &[("a.md", "name=alpha")]);
    assert!(run.warnings().is_empty(), "{:?}", run.warnings());

    index(&mut run, &store, &[("b.md", "name=alpha")]);
    // a.md takes it back later in the same run: a second takeover, but the
    // id has been reported already.
    index(&mut run, &store, &[("a.md", "name=alpha again")]);

    let collisions: Vec<&String> = run
        .warnings()
        .iter()
        .filter(|w| w.contains("pipeline::fake::alpha"))
        .collect();
    assert_eq!(collisions.len(), 1, "{:?}", run.warnings());
    assert!(
        collisions[0].contains("a.md") && collisions[0].contains("b.md"),
        "{}",
        collisions[0]
    );
    // The last document to write it owns it, as the id form dictates.
    let core = store
        .get_pipeline_core("pipeline::fake::alpha")
        .unwrap()
        .unwrap();
    assert_eq!(core.doc_id, "a.md");
}

#[test]
fn re_extracting_the_same_document_is_not_a_collision() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    let (_tmp, store) = store();
    let mut run = run_for(&env);

    index(&mut run, &store, &[("a.md", "name=alpha")]);
    index(&mut run, &store, &[("a.md", "name=alpha edited")]);
    assert!(run.warnings().is_empty(), "{:?}", run.warnings());
}

fn write(root: &std::path::Path, rel: &str, text: &str) {
    let full = root.join(rel);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

/// `DocIndex::index` on `root` with the quick timeouts, plugins from `env`.
fn index_dir(root: &std::path::Path) -> (DocIndex, infigraph_docs::DocIndexResult) {
    let mut idx = DocIndex::open(root).unwrap();
    idx.init().unwrap();
    idx.set_pipeline_timeouts(QUICK);
    let result = idx.index().unwrap();
    (idx, result)
}

#[test]
fn doc_index_stores_the_pipelines_of_the_documents_it_indexes() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    let project = tempfile::tempdir().unwrap();
    write(project.path(), "a.md", "name=alpha\nsome text");
    write(project.path(), "b.md", "plain prose");

    let (idx, result) = index_dir(project.path());
    assert_eq!(result.indexed_files, 2);
    assert!(
        result.pipeline_warnings.is_empty(),
        "{:?}",
        result.pipeline_warnings
    );

    let store = idx.store().unwrap();
    let cores = store.get_all_pipeline_cores(None).unwrap();
    assert_eq!(cores.len(), 1);
    assert_eq!(cores[0].id, "pipeline::fake::alpha");
    assert_eq!(cores[0].doc_id, "a.md");
}

/// Link-following indexes documents outside the doc root by a second
/// `upsert_docs`; those get pipelines too.
#[test]
fn doc_index_stores_the_pipelines_of_documents_found_by_following_links() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".git")).unwrap();
    write(
        repo.path(),
        "docs/index.md",
        "# Index\n\nSee [r](../README.md).\n",
    );
    write(repo.path(), "README.md", "# Readme\nname=linked\n");

    let (idx, result) = index_dir(&repo.path().join("docs"));
    assert_eq!(result.bfs_discovered, 1);

    let cores = idx.store().unwrap().get_all_pipeline_cores(None).unwrap();
    assert_eq!(cores.len(), 1, "{cores:?}");
    assert_eq!(cores[0].id, "pipeline::fake::linked");
    assert!(
        cores[0].doc_id.ends_with("README.md"),
        "{}",
        cores[0].doc_id
    );
}

/// A plugin that never answers must not fail or stall indexing: the documents
/// are indexed, the run costs one warning, and the plugin is not asked again.
#[test]
fn a_plugin_that_never_answers_costs_one_warning_and_no_documents() {
    let env = Env::new();
    env.plugin("fake", PATTERNS, SCRIPT);
    let project = tempfile::tempdir().unwrap();
    for n in 1..=3 {
        write(project.path(), &format!("d{n}.md"), &format!("HANG {n}"));
    }

    let started = std::time::Instant::now();
    let (idx, result) = index_dir(project.path());
    assert_eq!(result.indexed_files, 3);
    assert_eq!(idx.store().unwrap().get_doc_hashes().unwrap().len(), 3);
    assert_eq!(
        result.pipeline_warnings.len(),
        1,
        "{:?}",
        result.pipeline_warnings
    );
    assert!(result.pipeline_warnings[0].contains("fake"));
    // One request timeout, not three.
    assert!(
        started.elapsed() < Duration::from_millis(600 * 3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_plugin_command_that_does_not_exist_costs_one_warning_and_no_documents() {
    let env = Env::new();
    env.plugin_command("fake", PATTERNS, "[\"/nonexistent/infigraph-test-plugin\"]");
    let project = tempfile::tempdir().unwrap();
    for n in 1..=3 {
        write(project.path(), &format!("d{n}.md"), &format!("name=n{n}"));
    }

    let (idx, result) = index_dir(project.path());
    assert_eq!(result.indexed_files, 3);
    assert_eq!(idx.store().unwrap().get_doc_hashes().unwrap().len(), 3);
    assert_eq!(
        result.pipeline_warnings.len(),
        1,
        "{:?}",
        result.pipeline_warnings
    );
    assert!(result.pipeline_warnings[0].contains("fake"));
}
