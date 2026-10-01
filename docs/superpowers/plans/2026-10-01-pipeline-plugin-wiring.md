# Pipeline Plugins Run During Document Indexing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task, with superpowers:test-driven-development inside each task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a project's documents are indexed, each changed document is offered to the pipeline plugins, and what a plugin extracts is stored so `pipeline-query`, `pipeline-deps` and impact analysis return real rows. Today nothing calls the extractor or any pipeline write, so those readers always see empty tables.

**Architecture:** One new module, `infigraph_docs::pipelines`, owns a per-run `PipelineRun`: it loads the plugin registry once, starts a plugin lazily on its first matching document, writes through the existing `DocBackend` pipeline methods, and is dropped at the end of the run (which kills the plugin processes). `DocIndex::index` and `bfs_follow_links` call it right after `upsert_docs`, so every write is already under `.infigraph/docs-op.lock`. Deleting a document deletes its pipeline rows inside `delete_docs_by_ids`, so every deletion path is covered by one place. Which plugins may run is decided in `infigraph-pipeline-plugin` by one function.

**Tech Stack:** Rust, `infigraph_core::child` (`LineChild`, `ChildTimeouts`), `infigraph_core::settings!` with `ConfigScope::User`, lbug (Kuzu) via `DocStore`, the #204 `WriteRequest::IndexDocs` path (unchanged).

**No spec.** The user ruled "straight to plan" on 2026-10-01. The decisions a spec would carry are in "Decisions" below.

## Decisions

| # | Decision | Status |
|---|---|---|
| D1 | No spec; this plan is the design record. | User ruling, 2026-10-01 |
| D2 | **Trust.** User-level plugins (`~/.infigraph/pipelines`) run whenever document indexing runs. Project-level plugins (`<project>/pipelines`) run only for a project listed in `[pipelines] trusted_projects` of the **user** config layer (or its env override). A project's own `config.toml` can never enable them. | User ruling, 2026-10-01 |
| D3 | **Id form.** A pipeline's id is `pipeline::<plugin_id>::<name>`. | User ruling, 2026-10-01 |
| D4 | Hook point, lifecycle, failure policy, precedence and test shape as the peer proposed (Q1–Q5), with the changes written into the tasks. | Brainstorm ruling |

## Facts at e03e0d5 (verified)

- `PipelinePluginRegistry::extract_auto` (`crates/infigraph-pipeline-plugin/src/driver.rs:194`) has no caller. `load_pipeline_plugins` is called only by `cmd_pipeline_plugins` and `tool_pipeline_plugins`.
- `ensure_plugin_table`, `upsert_pipeline_core`, `upsert_plugin_properties`, `link_pipeline_core_to_doc` and `link_pipeline_dependencies` are on the `DocBackend` trait (`crates/infigraph-docs/src/backend.rs:64-78`) and are called only from `crates/infigraph-docs/tests/pipeline_core.rs`.
- `DocIndex::index` holds `store: &dyn DocBackend` and calls `store.upsert_docs` at `crates/infigraph-docs/src/lib.rs:324`; `bfs_follow_links` calls it at `:586`. `ExtractedDoc` carries `file`, `title: Option<String>` and `text`.
- `ops::index_docs` and the watcher's `reindex_if_enabled` both hold the docs lock exclusively before calling `index()`.
- `load_pipeline_plugins` registers user-level plugins first, then project-level, with no dedupe; `get_plugin` returns the first. `discover_and_register` iterates `read_dir` unsorted. `docs/PIPELINE_PLUGINS.md` says the project-level plugin wins.
- `extract_auto` uses `?` on `driver.extract`, so one failing plugin ends the scan. It never calls `start()`.
- `DocStore::delete_docs_by_ids` (`store.rs:386`) deletes `Chunk` and `Document` only.
- `PipelinePluginDriver::start_with` runs `command` with `current_dir(plugin_dir)`.
- `upsert_plugin_properties` (`store.rs:503`) writes every property as a quoted string, whatever the column's declared type. Unverified whether lbug accepts that for `INT64`/`BOOL`/`DOUBLE`/`STRING[]`; Task 2 measures it.
- `infigraph-pipeline-plugin` depends on `infigraph-core` only. `infigraph-docs` does not yet depend on `infigraph-pipeline-plugin`; adding it creates no cycle.

Not re-verified by brainstorm (peer's greps): the Confluence call sites `crates/infigraph-confluence/src/sync.rs:131` and `crates/infigraph-mcp/src/tools/docs.rs:513`. Task 6 reads them first.

## Global Constraints

- DRY is the #1 rule. One function decides which plugins may run. One function writes a document's pipeline rows. One function deletes them.
- No new lock. Every pipeline write happens inside a call that already holds `docs-op.lock` exclusively. A new call site that does not hold it is a stop.
- No new spawn path. Plugins are started only through `PipelinePluginDriver::start_with`, which uses `infigraph_core::child`.
- New lbug writes are bracketed with `write_phase::enter(&"…", n)` and tick `growth_gate` once per document batch (AGENTS.md, #132/#153).
- A pipeline failure never fails a document index run. It warns on stderr and the run continues.
- A read never starts a plugin. `cmd_pipeline_plugins` and `tool_pipeline_plugins` keep listing without spawning.
- Remote mode (`INFIGRAPH_BACKEND=neo4j`) goes through the same helper via `DocBackend`; no remote-only code is added.
- Tests pin the backend and home: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p <crate> … -- --test-threads=1`, and use `settings_file::test_support::PinnedHome` so no real user-level plugin or config is read. CLI integration tests need `cargo build -p infigraph-cli` first.
- Fixture plugins are `sh` scripts speaking the JSON line protocol; gate those tests `#[cfg(unix)]`.
- Branch: `fix/pipeline-plugin-wiring` in `scratchpad/wt-pipeline-wiring`. It is at d846675; bring it to the tip of `feat/hardening` (ee02c10 or later; it carries the `scip` 0.10.0 bump on top of e03e0d5, where the facts above were verified) before Task 1. Never push or merge; brainstorm reviews each task, the user says when to merge.
- Intermediate commits may use `--no-verify`; Task 7 runs the full hook. End every commit message with your session's attribution lines.

## Review Focus

1. **A cloned repo must not get its commands run.** With an empty `trusted_projects`, a project-level plugin whose `command` would create a marker file is indexed and the marker is absent. Pinned in Task 1 and again end to end in Task 5.
2. **A hung or dead plugin must not hang or fail indexing.** One warning per plugin per run, not one per document. Pinned in Task 4.
3. **No orphan rows.** Editing a document so it stops matching, renaming its pipeline, and deleting the document each leave zero rows for the old id in both `PipelineCore` and `Pipeline_<plugin_id>`. Pinned in Tasks 2 and 5.
4. **A plugin error keeps the previous rows; a plugin `skip` deletes them.** These are different outcomes and both are tested.
5. **An unchanged project spawns nothing.** A second `index-docs` with no document change starts no plugin process. Pinned in Task 5.

---

## Task 1: One function decides which plugins may run

**Files:** `crates/infigraph-pipeline-plugin/src/lib.rs`, `src/driver.rs`; tests in the same crate.

- [ ] Add a `settings!` group `pipelines { trusted_projects: PathList = PathList(Vec::new()) }`, resolved with `ConfigScope::User` only, the way `scip_slots` resolves `[scip] max_concurrent_indexers`. Compare canonicalized paths.
- [ ] Change `load_pipeline_plugins` to take the project root (`Option<&Path>`) instead of a pipelines directory, derive `<root>/pipelines` itself, and load it only when the root is trusted. Update the two listing callers; they now show exactly the plugins that would run, and print one line naming an untrusted project directory that was skipped and the setting that enables it.
- [ ] Dedupe by `plugin_id` with project-level winning over user-level, then sort by `plugin_id`. Update `discover_and_register` or the registry so the result is deterministic.
- [ ] Tests (write first, watch fail): untrusted project plugin is not loaded; trusted one is; a project-level `[pipelines]` table in `<project>/.infigraph/config.toml` does not enable it; same `plugin_id` in both layers yields the project-level one; order is sorted.

## Task 2: Store side — delete by document, typed properties

**Files:** `crates/infigraph-docs/src/backend.rs`, `store.rs`, `neo4j_store.rs`, `daemon_store.rs`; tests in `crates/infigraph-docs/tests/pipeline_core.rs`.

- [ ] Test first: after `upsert_pipeline_core` + `upsert_plugin_properties` + `link_pipeline_core_to_doc` for a document, `delete_docs_by_ids(&[doc])` leaves no `PipelineCore` row and no `Pipeline_<plugin_id>` row for it.
- [ ] Add one `DocBackend` method, `delete_pipelines_for_docs(&self, doc_ids: &[&str]) -> Result<()>`: find cores by `doc_id`, delete each one's `Pipeline_<plugin_id>` row by id, then `DETACH DELETE` the cores. Call it from inside `delete_docs_by_ids` in `DocStore` and `Neo4jDocStore`. `DaemonDocStore` answers `writes_not_routed` like its siblings.
- [ ] Test first: a plugin table with one column of each of `STRING`, `INT64`, `BOOL`, `DOUBLE`, `STRING[]` round-trips through `upsert_plugin_properties` and `query_plugin_table`. If quoted-string writes fail for typed columns, fix `upsert_plugin_properties` to emit a literal per declared type; if they pass, record the measured result in the commit message and change nothing.
- [ ] Schema drift: `ensure_plugin_table` uses `CREATE … IF NOT EXISTS` and will not alter a table. Make it return an error naming the plugin and "run `infigraph reindex-docs`" when the existing table's columns differ from the plugin's schema. Test it.
- [ ] Bracket the new writes with `write_phase::enter`.

## Task 3: `PipelineRun` — the one writer

**Files:** create `crates/infigraph-docs/src/pipelines.rs`; `crates/infigraph-docs/Cargo.toml` (add `infigraph-pipeline-plugin`); `crates/infigraph-docs/src/lib.rs` (module only).

- [ ] `PipelineRun::for_project(root: &Path, timeouts: ChildTimeouts) -> PipelineRun`. Loads the registry through Task 1's function. A load error is a warning and an empty run. `timeouts` is the test seam; production passes `ChildTimeouts::DEFAULT`.
- [ ] `PipelineRun::apply(&mut self, store: &dyn DocBackend, docs: &[&ExtractedDoc])`. For each document, in sorted plugin order, for the first plugin with a matching `detect_patterns` regex (compile each plugin's regexes once per run, warn once on an invalid one):
  - start the plugin if this run has not started it; a start failure marks it dead for the run with one warning;
  - `ok` → delete the document's existing pipeline rows (`delete_pipelines_for_docs`), `ensure_plugin_table` (once per plugin per run), `upsert_pipeline_core` with id `pipeline::<plugin_id>::<name>` (D3), `upsert_plugin_properties`, `link_pipeline_core_to_doc`;
  - `skip` → try the next matching plugin; if none produces data, delete the document's existing rows;
  - error or timeout → warn naming plugin and document, keep the document's existing rows, mark the plugin dead for the run if the driver is poisoned, and do not try other plugins for this document.
  - A document no plugin matches gets its existing rows deleted.
- [ ] `apply` returns nothing that can fail the caller. It records whether any row changed.
- [ ] `PipelineRun::finish(self, store)`: if any row changed, `link_pipeline_dependencies()` once. Dropping the run drops the registry, which kills the plugin process groups.
- [ ] Replace `extract_auto`'s body or remove it so there is one scan loop, not two. `PipelinePluginRegistry::extract` (by id) stays.
- [ ] One `growth_gate` tick per `apply` call.
- [ ] Unit tests with a fixture `sh` plugin and a temp `DocStore`: ok, skip-then-delete, error-keeps-rows, renamed pipeline leaves no old id, two plugins with the same pipeline name do not overwrite each other.

## Task 4: Hook into indexing

**Files:** `crates/infigraph-docs/src/lib.rs` (`DocIndex::index`, `bfs_follow_links`).

- [ ] In `index`, create the `PipelineRun` only when `results` is non-empty or the link-following pass indexes something, call `apply` right after `store.upsert_docs(&docs, &chunks)?` (`lib.rs:324`), pass the same run into `bfs_follow_links` and call `apply` after its successful `upsert_docs` (`:586`), and `finish` before returning. The project root for plugin loading is `infigraph_core::project::resolve_project_root(&self.root)`.
- [ ] `DocIndex` gains a `ChildTimeouts` field defaulting to `DEFAULT`, with a setter used only by tests.
- [ ] Tests at `DocIndex` level: a plugin that never answers, with a 1s request timeout, leaves `index()` returning `Ok` with the documents indexed and exactly one warning for that plugin across three matching documents; a plugin whose command does not exist behaves the same.

## Task 5: End-to-end test through the real CLI

**Files:** create `crates/infigraph-cli/tests/pipeline_plugins.rs`, using `tests/support`.

- [ ] Fixture: a temp project with `docs/pipeline.md` matching `detect_patterns`, a user-level plugin under a pinned `HOME` (`$HOME/.infigraph/pipelines/fake/plugin.toml` plus an `extract.sh`), schema with one searchable `STRING` field.
- [ ] Watch it fail at e03e0d5 first, then pass: `infigraph index-docs` under the default daemon backend, then `infigraph pipeline-query fake <field> <value>` and `infigraph pipeline-deps` show the row. Repeat the index step under `INFIGRAPH_BACKEND=kuzu`.
- [ ] Edit the document so the plugin returns a different pipeline name: the old id is gone. Delete the document and index again: no rows remain.
- [ ] Second `index-docs` with no change: the fixture plugin appends to a file each time it starts; the count does not grow.
- [ ] Trust: add a project-level plugin whose command creates a marker file; with no `trusted_projects`, index and assert the marker is absent; add the project to the pinned user config and assert it is present.

## Task 6: Confluence paths

**Files:** `crates/infigraph-confluence/src/sync.rs`, `crates/infigraph-mcp/src/tools/docs.rs`.

- [ ] Read both call sites first. If each has the `ExtractedDoc`s and the store in hand and holds the docs lock exclusively, call the same `PipelineRun` after `upsert_docs`. `remove_deleted_pages` already goes through `delete_docs_by_ids`, so Task 2 covers its cleanup.
- [ ] If either site does not hold the docs lock exclusively, stop and report; do not add a lock here.

## Task 7: Docs and gates

**Files:** `docs/PIPELINE_PLUGINS.md`, `docs/DOCUMENT-INDEXING.md`, `AGENTS.md`.

- [ ] `PIPELINE_PLUGINS.md`: when plugins run, the trust rule and the setting, the id form (update the example), failure behavior, the schema-change message, and the override rule as now implemented.
- [ ] `AGENTS.md`: one invariant line under "Cross-cutting invariants": pipeline plugins run only inside a document index run, under the docs lock; a project's own plugins run only when the user layer trusts the project.
- [ ] Gates, each as its own shell call, reported as numbers: `cargo fmt --all -- --check`; `cargo clippy --all-targets -- -D warnings`; per-crate tests for `infigraph-pipeline-plugin`, `infigraph-docs`, `infigraph-confluence`, `infigraph-cli`, `infigraph-mcp` with `--test-threads=1`; then `cargo test --all`; then the full pre-commit hook.

## Stop rules

Stop and report to brainstorm, without committing, when:

- a pipeline write would run outside the exclusive docs lock;
- typed property writes need a change to the `DocBackend` trait beyond Task 2;
- an existing test changes its expected output under a task meant to leave it alone;
- the Confluence sites differ from the description in Task 6;
- anything would start a plugin from a read path.

## Follow-ups (issues, not this branch)

- A `doctor` line listing plugins that would run for a project and any skipped as untrusted.
- A protocol `shutdown` command so a plugin can exit cleanly instead of being killed.
