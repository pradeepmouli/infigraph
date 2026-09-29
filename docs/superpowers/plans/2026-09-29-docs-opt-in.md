# Document Indexing Is Opt-In Per Project — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A project gets a document index only after it opts in (`index-docs`, or `[docs] enabled = true`). `index-docs` works under the default daemon backend by asking the daemon to index. `clean-docs` opts the project out and it stays out.

**Architecture:** One switch, `[docs] enabled`, is a `settings!` group in a new `infigraph_core::docs_switch` module. `docs_enabled(root)` is the only question anything asks. One new writer, `settings_file::set_project_setting`, records it. `write_watch_policy` becomes a caller of that writer. The daemon's docs row source opens `docs.kuzu` only when the file exists, per request. The doc thread attaches on the switch rather than on the file. A new `WriteRequest::IndexDocs { full }` reaches the executor `infigraph_docs::ops::index_docs` through the existing `DocsHandle` trait, which `cmd_daemon` already injects into the coordinator because `infigraph-core` cannot depend on `infigraph-docs`. The coordinator runs it on a background `Task`, like `ScipImport`. A cross-process lock, `.infigraph/docs-op.lock`, is taken exclusively by every operation that may create or delete the store (the watcher's reindex, `index_docs`, `clean_docs`) and shared by every operation that needs the store to exist and stay (reads). Readers go through `DocIndex::open_existing` or `DocStore::open_for_read`, which never create a store.

**Tech Stack:** Rust, `toml_edit` (re-exported as `infigraph_core::toml_edit`), `settings!`, `infigraph_core::lockfile` (fs2 flock), the #204 socket write path (`daemon::writes::submit`, `WriteReply`, `route_write`, `Task::spawn_blocking`), lbug (Kuzu) via `DocStore`.

**Spec:** `docs/superpowers/specs/2026-09-29-docs-opt-in-design.md`. Read it before any task. It carries the user's decisions (opt-in per project, existing indexes stay on, approach A) and the reasons this plan does not repeat. Where this plan departs from it, the task says why. The departures are also listed under "Departures from the spec" below.

## Global Constraints

- Setting: `[docs] enabled`, default `false`, env override `INFIGRAPH_DOCS_ENABLED` (`settings::Toggle`: anything but `0`/`false` is on). It resolves like every group: CLI > env > project `config.toml` > user `~/.infigraph/config.toml` > default.
- Exactly one function answers "is doc indexing on here?": `infigraph_core::docs_switch::docs_enabled(root: &Path) -> bool`. Nothing else reads `[docs] enabled`, and nothing checks for `docs.kuzu` to decide whether docs are on.
- Exactly three writers of the switch, all through `docs_switch::set_docs_enabled`: the executor `infigraph_docs::ops::index_docs` (`true`), `infigraph_docs::ops::clean_docs` (`false`), and `docs_switch::migrate_existing_index` (`true`, only when the key is absent). Nothing hand-edits TOML.
- `settings_file::set_project_setting` is the only code that writes a project's `config.toml`. It keeps comments, formatting and other keys, and writes through `daemon_protocol::write_atomic`.
- Remote/Postgres mode (`INFIGRAPH_BACKEND=neo4j`) is unchanged: `cmd_index_docs`'s remote branch keeps its direct write path, and the remote group build keeps its per-repo loop.
- Stores open only in the daemon. `INFIGRAPH_BACKEND=kuzu` is the sole direct path. `INFIGRAPH_DIRECT_READS=1` is the existing explicit read hatch and keeps working.
- A read never creates `docs.kuzu` or `.infigraph/`. A reader checks for the store before it takes any lock, because taking a lock file creates its parent directory.
- `.infigraph/docs-op.lock` (`docs_switch::DOCS_OP_LOCK`): exclusive for operations that may create or delete the store, shared for operations that need it to exist. The doc watcher takes it with `try_lock_docs_op` and never blocks on it. Everything else waits up to `docs_switch::DOCS_OP_WAIT` (600s, `FullReindex`'s client budget).
- The executor adds no new lbug open or write. It reuses `DocIndex::init`/`index`/`reindex`, so no new `write_phase::enter` bracket is needed. If an implementer adds a direct `DocStore`/`Database` open or COPY anywhere, it must be bracketed per CLAUDE.md.
- DRY is the #1 rule. The not-indexed message is `docs_switch::DOCS_NOT_INDEXED` everywhere. The store path is `docs_switch::docs_store_path(root)` in all new code.
- Tests pin the backend and clear the leaking variables: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p <crate> … -- --test-threads=1`. `infigraph-cli` has no lib target: its unit tests run with `--bin infigraph`. Its integration tests that spawn the binary through `cli_binary()` need `cargo build -p infigraph-cli` first. Tests that use `env!("CARGO_BIN_EXE_infigraph")` get it built by cargo.
- This machine is disk-constrained. Run per crate, never one `cargo test --all` until Task 10.
- Intermediate task commits use `git commit --no-verify`, as #204's plan did. The hook's perf gates do not touch these paths. Task 10 runs the full hook.
- Every commit message ends with:
  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV
  ```

## Review Focus

1. **A `config.toml` that cannot take the write**: a TOML syntax error, or a `docs = 5` whose `[docs]` is not a table. `toml_edit`'s `IndexMut` panics on a non-table. The migration, `index-docs` and `clean-docs` must fail with a message and leave the file byte-for-byte as it was. They must never panic and never write a fresh file over it. Pinned in Task 1: `set_project_setting_refuses_a_section_that_is_not_a_table`. Pinned in Task 2: `migration_leaves_an_unparseable_config_alone`.
2. **A present but invalid `[docs] enabled = "yes"`**: the migration's "absent" test must be about the key, not about its parsed value. Otherwise it overwrites the user's (bad) choice with `true`. `docs_enabled` falls back to off with a one-time warning, and `doctor`'s existing settings check names the key. Pinned in Task 2: `migration_leaves_a_present_but_invalid_value_alone`.
3. **`clean-docs` racing the doc watcher, and a full `IndexDocs` racing it.** `DB_LOCK` is process-local and `DocIndex::clean` takes no lock at all. Without the docs lock, `clean-docs` (CLI process) or `reindex()`'s `clean()` can unlink the store under the watcher's open handle. The watcher's next open then recreates it. Pinned in Task 7: `clean_docs_is_not_undone_by_an_attached_watcher`. Pinned in Task 4: `index_docs_waits_for_a_docs_operation_already_running`.
4. **A store created after the daemon started** (the first `index-docs`, or the watcher's first catch-up) must be served without a restart. The old probe registered no docs source at all when its open failed. A read of a project with no `.infigraph/` must not create one: the removed-worktree incident was exactly that. Pinned in Task 3: `a_store_created_after_the_source_is_served` and `a_read_of_a_project_with_no_infigraph_dir_creates_nothing`.
5. **An executor that panics or fails** must still answer its client, and must never hang it until its 600s timeout. A second `IndexDocs` that arrives while one runs must wait its turn, not run alongside. Pinned in Task 5: `a_panicking_docs_index_still_answers_its_client` and `a_second_index_docs_waits_for_the_running_one`.

## Departures from the spec

- **The `docs` settings group lives in `infigraph-core` (`docs_switch`), not `infigraph-docs`.** `doctor` (in `infigraph-core`) must call `docs_enabled`, and core cannot depend on docs. The migration and the docs lock live beside it for the same reason.
- **`IndexDocs` carries no `namespace`.** `index-docs` has no `--namespace` flag. A namespace exists only in remote mode (`Registry::resolve_repo_namespace`), and remote mode keeps its own direct path. No local caller could send one.
- **The core-to-docs injection is the existing `DocsHandle` trait, extended with `index_docs(&self, full: bool)`.** It is not a new closure parameter on `run_write_coordinator`. `cmd_daemon` already hands the coordinator a `DocsHandle` (`DocWatchHandle`) for exactly this crate-boundary reason. The method is required, with no default, so a handle cannot silently forget it.
- **A cross-process docs lock (`docs-op.lock`) is added.** The spec's concurrency argument ("serialized by `DB_LOCK`") holds only within one process. It does not cover `clean-docs` (CLI process) or `DocIndex::reindex`, whose `clean()` deletes files without any lock.
- **Readers get their own entry points (`DocIndex::open_existing`, `DocStore::open_for_read`).** The spec's Review Focus 1 asked for an audit of `DocStore::open`, but the widest opener is `DocIndex::init`, which calls it. `init` is reached from about fifteen reader or incidental sites. One is MCP `search` with `scope = "all"`, which runs on every search; without the daemon (`INFIGRAPH_BACKEND=kuzu`) it created a store for every project it searched.
- **Group build step 5 (local) now indexes only opted-in repos, through `request_index_docs`.** It used to run `DocIndex::init` + `index` on every processed repo, which created a store for each one (or failed under the daemon backend).
- **`clean-docs`, `reindex-docs` and MCP's no-CLI fallbacks change too.** The spec calls MCP `index_docs`/`clean_docs` "unchanged", which holds for their CLI-subprocess path only. Their in-process fallbacks now call the same `ops` functions.
- **Confluence indexing (CLI and MCP) needs an opted-in project.** It writes into an existing store and uses `open_existing`, so on a fresh project it answers `DOCS_NOT_INDEXED`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/infigraph-core/src/settings_file.rs` (modify) | `project_setting` (raw read), `set_project_setting` (the one writer), `test_support` (`ENV_LOCK`, `PinnedHome`, moved from `watch::config`'s tests) |
| `crates/infigraph-core/src/watch/config.rs` (modify) | `write_watch_policy` delegates to `set_project_setting`; tests use `test_support` |
| `crates/infigraph-core/src/docs_switch.rs` (create) | `settings! docs`, `docs_enabled`, `docs_indexed`, `docs_enabled_recorded`, `set_docs_enabled`, `migrate_existing_index`, `DOCS_NOT_INDEXED`/`DocsNotIndexed`, `docs_store_path`, the docs lock |
| `crates/infigraph-core/src/lib.rs` (modify) | `pub mod docs_switch;` |
| `crates/infigraph-core/src/ps.rs` (modify) | `docs-op.lock` in `PROJECT_LOCKS` |
| `crates/infigraph-core/src/daemon_protocol.rs` (modify) | `DocIndexStats`, `WriteRequest::IndexDocs`, `WriteResult::DocsIndexed`, `serve_write` arm |
| `crates/infigraph-core/src/daemon/mod.rs` (modify) | `DocsHandle::index_docs`, `NO_DOCS_INDEXER`, `PendingDocsIndex`, `try_start_docs_index`, `finish_docs_index`, coordinator wiring, `Router` test harness |
| `crates/infigraph-core/src/doctor.rs` (modify) | `check_docs` |
| `crates/infigraph-docs/src/ops.rs` (create) | `index_docs` (the executor), `request_index_docs`, `request_index_docs_if_enabled`, `clean_docs`, `stats_report` |
| `crates/infigraph-docs/src/lib.rs` (modify) | `pub mod ops;`, `DocIndex::open_existing`, `read_lock` field |
| `crates/infigraph-docs/src/store.rs` (modify) | `DocStore::open_for_read`, `DocStoreRead` |
| `crates/infigraph-docs/src/daemon_source.rs` (modify) | Probe and per-request read through `open_for_read` |
| `crates/infigraph-docs/src/daemon_store.rs` (modify) | Direct reads through `open_for_read`; the write refusal names `index-docs` |
| `crates/infigraph-docs/src/watch.rs` (modify) | Attach on the switch; reindex under the docs lock, re-checking the switch |
| `crates/infigraph-docs/src/combined.rs` (modify) | Source repos only when enabled; `combined_doc_search` never creates |
| `crates/infigraph-cli/src/info_commands.rs` (modify) | Migration at daemon start; `DocWatchHandle::index_docs`; `index-docs`/`reindex-docs`/`clean-docs`; manifests and confluence readers |
| `crates/infigraph-cli/src/main.rs`, `search_commands.rs`, `pipeline_commands.rs`, `group_commands.rs` (modify) | Callers |
| `crates/infigraph-mcp/src/tools/{docs,index,groups,pipelines}.rs` (modify) | The auto-start gate, readers, fallbacks, group build |
| `docs/DOCUMENT-INDEXING.md`, `CLAUDE.md`, `AGENTS.md` (modify) | Documentation and the invariant |

---

### Task 1: The switch, the config writer, the docs lock

**Files:**
- Modify: `crates/infigraph-core/src/settings_file.rs` (add after `layers`, L105-113; tests module L173-242; new `test_support` module at the end of the file)
- Modify: `crates/infigraph-core/src/watch/config.rs` (`write_watch_policy` L85-103; tests L105-152 lose `ENV_LOCK` and `PinnedHome`)
- Create: `crates/infigraph-core/src/docs_switch.rs`
- Modify: `crates/infigraph-core/src/lib.rs:16` (add `pub mod docs_switch;` above `pub mod doctor;`)
- Modify: `crates/infigraph-core/src/ps.rs:47-53` (`PROJECT_LOCKS`)

**Interfaces:**
- Consumes: `settings_file::{project_config_path, load, layers, ConfigScope}`, `daemon_protocol::write_atomic(path: &Path, contents: &str) -> anyhow::Result<()>`, `lockfile::{acquire, acquire_shared, try_acquire, LockFile}`, `settings::Toggle`.
- Produces:
  - `pub fn settings_file::project_setting(root: &Path, section: &str, key: &str) -> Option<toml_edit::Item>`
  - `pub fn settings_file::set_project_setting(root: &Path, section: &str, key: &str, value: toml_edit::Item) -> anyhow::Result<()>`
  - `#[cfg(test)] pub(crate) mod settings_file::test_support { pub(crate) static ENV_LOCK: Mutex<()>; pub(crate) struct PinnedHome; PinnedHome::empty() -> Self; PinnedHome::with(section: &str, enabled: bool) -> Self }`
  - `docs_switch::{Docs, RawDocs}` (the `settings!` group)
  - `pub const docs_switch::DOCS_NOT_INDEXED: &str`, `pub struct docs_switch::DocsNotIndexed` (`Display` = `DOCS_NOT_INDEXED`, `std::error::Error`)
  - `pub const docs_switch::DOCS_OP_LOCK: &str = "docs-op.lock"`, `pub const docs_switch::DOCS_OP_WAIT: Duration`
  - `pub fn docs_switch::docs_store_path(root: &Path) -> PathBuf`
  - `pub fn docs_switch::docs_enabled(root: &Path) -> bool`
  - `pub fn docs_switch::docs_indexed(root: &Path) -> bool` (enabled **and** the store exists)
  - `pub fn docs_switch::docs_enabled_recorded(root: &Path) -> Option<bool>`
  - `pub fn docs_switch::set_docs_enabled(root: &Path, enabled: bool) -> anyhow::Result<()>`
  - `pub fn docs_switch::lock_docs_op(root: &Path, timeout: Duration) -> anyhow::Result<LockFile>` (exclusive)
  - `pub fn docs_switch::try_lock_docs_op(root: &Path) -> anyhow::Result<Option<LockFile>>` (exclusive, non-blocking)
  - `pub fn docs_switch::lock_docs_read(root: &Path) -> anyhow::Result<LockFile>` (shared, waits `DOCS_OP_WAIT`)

- [ ] **Step 1: Write the failing `settings_file` tests.** Append to `settings_file.rs`'s `mod tests`:

```rust
    #[test]
    fn set_project_setting_keeps_comments_and_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(
            tmp.path(),
            "# my settings\n[compression]\nlevel = \"aggressive\" # keep me\n\n[docs]\nother = 1\n",
        );
        set_project_setting(tmp.path(), "docs", "enabled", toml_edit::value(true)).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my settings"), "{text}");
        assert!(text.contains("level = \"aggressive\" # keep me"), "{text}");
        assert!(text.contains("other = 1"), "{text}");
        assert_eq!(
            project_setting(tmp.path(), "docs", "enabled").and_then(|i| i.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn set_project_setting_creates_the_file_when_there_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        set_project_setting(tmp.path(), "docs", "enabled", toml_edit::value(false)).unwrap();
        assert_eq!(
            project_setting(tmp.path(), "docs", "enabled").and_then(|i| i.as_bool()),
            Some(false)
        );
    }

    /// Review Focus 1: `toml_edit` panics indexing into a non-table, so a
    /// `docs = 5` must be refused before the write, and left as it was.
    #[test]
    fn set_project_setting_refuses_a_section_that_is_not_a_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(tmp.path(), "docs = 5\n");
        let err = set_project_setting(tmp.path(), "docs", "enabled", toml_edit::value(true))
            .unwrap_err();
        assert!(err.to_string().contains("not a table"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "docs = 5\n");
    }

    #[test]
    fn project_setting_is_none_for_an_absent_key_or_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(project_setting(tmp.path(), "docs", "enabled").is_none());
        write_config(tmp.path(), "[docs]\nother = 1\n");
        assert!(project_setting(tmp.path(), "docs", "enabled").is_none());
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib settings_file -- --test-threads=1`
Expected: FAIL to compile (`set_project_setting` and `project_setting` not found).

- [ ] **Step 3: Implement the reader and the writer.** In `settings_file.rs`, add `use anyhow::Context;` to the imports. Then add after `layers`:

```rust
/// One key as the project's own `config.toml` states it, or `None` when
/// the file is missing or unparseable, or says nothing about the key. It
/// reads the raw item, not a resolved value, so a caller can tell "absent"
/// from "set to something" (the docs migration needs exactly that).
pub fn project_setting(root: &Path, section: &str, key: &str) -> Option<toml_edit::Item> {
    load(&project_config_path(root))?
        .get(section)?
        .get(key)
        .cloned()
}

/// The one writer of a project's `config.toml`: sets `[section] key =
/// value` and leaves every other section, key, comment and format alone.
///
/// A missing file starts from an empty document, since there is nothing to
/// keep. A file that exists but does not parse, or whose `section` is not a
/// table, is an error and is left untouched. Writing a fresh document over
/// it would silently drop every other setting the moment it had a typo.
pub fn set_project_setting(
    root: &Path,
    section: &str,
    key: &str,
    value: toml_edit::Item,
) -> anyhow::Result<()> {
    let config_path = project_config_path(root);
    let mut doc: toml_edit::DocumentMut = match std::fs::read_to_string(&config_path) {
        Ok(contents) => contents
            .parse()
            .with_context(|| format!("{} contains invalid TOML", config_path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml_edit::DocumentMut::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", config_path.display()));
        }
    };
    if doc.get(section).is_some_and(|item| !item.is_table_like()) {
        anyhow::bail!(
            "{}: `{section}` is not a table, so [{section}] {key} cannot be set",
            config_path.display()
        );
    }
    doc[section][key] = value;
    crate::daemon_protocol::write_atomic(&config_path, &doc.to_string())
}
```

- [ ] **Step 4: Move the test guards and make `write_watch_policy` a caller.** Append to `settings_file.rs`:

```rust
/// One env lock and one `$HOME` pin for every test in this crate that reads
/// the user layer or sets an `INFIGRAPH_*` variable. A lock per module does
/// not stop two modules racing on the same process-global `HOME`.
#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Pins `$HOME` at an empty directory for the guard's lifetime. The
    /// user layer is real config, so a test that does not do this reads
    /// whatever the developer running it has in `~/.infigraph/config.toml`.
    pub(crate) struct PinnedHome {
        _dir: tempfile::TempDir,
        orig: Option<String>,
    }

    impl PinnedHome {
        pub(crate) fn empty() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let orig = std::env::var("HOME").ok();
            std::env::set_var("HOME", dir.path());
            Self { _dir: dir, orig }
        }

        pub(crate) fn with(section: &str, enabled: bool) -> Self {
            let pinned = Self::empty();
            let ig = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".infigraph");
            std::fs::create_dir_all(&ig).unwrap();
            std::fs::write(
                ig.join("config.toml"),
                format!("[{section}]\nenabled = {enabled}\n"),
            )
            .unwrap();
            pinned
        }
    }

    impl Drop for PinnedHome {
        fn drop(&mut self) {
            match &self.orig {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
```

In `watch/config.rs`, replace the body of `write_watch_policy` (L85-103) with:

```rust
pub fn write_watch_policy(root: &Path, role: WatchRole, enabled: bool) -> Result<()> {
    let section = section_for_role(role)?;
    crate::settings_file::set_project_setting(root, section, "enabled", toml_edit::value(enabled))
}
```

Change the import line to `use crate::daemon_protocol::WatchRole;`, and drop `Context` from `use anyhow::{Context, Result};` if nothing else in the file uses it. In its `mod tests`, delete `static ENV_LOCK` and the whole `PinnedHome` struct, its `impl` and its `Drop` (L110-152), and add `use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};`. The test bodies do not change. `write_watch_policy_rejects_malformed_config_instead_of_clobbering_it` keeps passing because the message still says "invalid TOML".

- [ ] **Step 5: Run the settings and watch-config tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib settings_file watch::config -- --test-threads=1`
Expected: PASS, the four new tests and every existing `watch::config` test included.

- [ ] **Step 6: Write the failing `docs_switch` tests.** Create `crates/infigraph-core/src/docs_switch.rs` containing only the tests module for now, and add `pub mod docs_switch;` to `lib.rs` above `pub mod doctor;`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};

    const ENV: &str = "INFIGRAPH_DOCS_ENABLED";

    #[test]
    fn docs_are_off_by_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        std::env::remove_var(ENV);
        let tmp = tempfile::tempdir().unwrap();
        assert!(!docs_enabled(tmp.path()));
        assert_eq!(docs_enabled_recorded(tmp.path()), None);
    }

    #[test]
    fn the_project_config_turns_docs_on_and_off() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        std::env::remove_var(ENV);
        let tmp = tempfile::tempdir().unwrap();
        set_docs_enabled(tmp.path(), true).unwrap();
        assert!(docs_enabled(tmp.path()));
        assert_eq!(docs_enabled_recorded(tmp.path()), Some(true));
        set_docs_enabled(tmp.path(), false).unwrap();
        assert!(!docs_enabled(tmp.path()));
        assert_eq!(docs_enabled_recorded(tmp.path()), Some(false));
    }

    #[test]
    fn the_env_overrides_the_project_config() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        set_docs_enabled(tmp.path(), true).unwrap();
        std::env::set_var(ENV, "0");
        assert!(!docs_enabled(tmp.path()));
        set_docs_enabled(tmp.path(), false).unwrap();
        std::env::set_var(ENV, "1");
        assert!(docs_enabled(tmp.path()));
        std::env::remove_var(ENV);
    }

    /// The user layer is a layer (#160): a machine-wide `[docs] enabled =
    /// true` turns docs on for every project that states nothing itself.
    #[test]
    fn the_user_layer_applies_when_the_project_is_silent() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV);
        let _home = PinnedHome::with("docs", true);
        let tmp = tempfile::tempdir().unwrap();
        assert!(docs_enabled(tmp.path()));
        set_docs_enabled(tmp.path(), false).unwrap();
        assert!(!docs_enabled(tmp.path()), "the project wins the key it states");
    }

    #[test]
    fn docs_indexed_needs_both_the_switch_and_the_store() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        std::env::remove_var(ENV);
        let tmp = tempfile::tempdir().unwrap();
        let store = docs_store_path(tmp.path());
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, b"x").unwrap();
        assert!(!docs_indexed(tmp.path()), "a store alone is not opted in");
        set_docs_enabled(tmp.path(), true).unwrap();
        assert!(docs_indexed(tmp.path()));
        std::fs::remove_file(&store).unwrap();
        assert!(!docs_indexed(tmp.path()), "the switch alone has nothing to read");
    }

    #[test]
    fn the_docs_lock_admits_one_writer_and_excludes_readers() {
        let tmp = tempfile::tempdir().unwrap();
        let held = lock_docs_op(tmp.path(), Duration::from_secs(1)).unwrap();
        assert!(try_lock_docs_op(tmp.path()).unwrap().is_none());
        drop(held);
        let reader = lock_docs_read(tmp.path()).unwrap();
        assert!(
            try_lock_docs_op(tmp.path()).unwrap().is_none(),
            "a reader keeps a writer out"
        );
        drop(reader);
        assert!(try_lock_docs_op(tmp.path()).unwrap().is_some());
    }

    #[test]
    fn not_indexed_reads_as_its_message() {
        assert_eq!(DocsNotIndexed.to_string(), DOCS_NOT_INDEXED);
        let err = anyhow::Error::new(DocsNotIndexed);
        assert!(err.is::<DocsNotIndexed>());
    }
}
```

- [ ] **Step 7: Run them to see them fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib docs_switch -- --test-threads=1`
Expected: FAIL to compile (`docs_enabled`, `set_docs_enabled` and the rest not found).

- [ ] **Step 8: Implement `docs_switch`.** Put this above the tests module:

```rust
//! Whether a project indexes its documents: the `[docs] enabled` switch.
//!
//! Document indexing is opt-in per project
//! (docs/superpowers/specs/2026-09-29-docs-opt-in-design.md). Every "is doc
//! indexing on here?" question is [`docs_enabled`]. Three things record the
//! switch, all through [`set_docs_enabled`]: `index-docs` turns it on,
//! `clean-docs` turns it off, and the daemon-start migration turns it on for
//! an index that predates the switch.
//!
//! It lives here rather than in `infigraph-docs` because `doctor`, in this
//! crate, reports it, and this crate cannot depend on that one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::settings_file::{self, ConfigScope};

crate::settings! {
    docs {
        enabled: crate::settings::Toggle = crate::settings::Toggle(false),
    }
}

const SECTION: &str = "docs";
const ENABLED: &str = "enabled";

/// What a read of a project with no document index answers, everywhere:
/// the daemon's read service, `search-docs`, MCP `search_docs`.
pub const DOCS_NOT_INDEXED: &str =
    "documents are not indexed for this project; run `infigraph index-docs`";

/// [`DOCS_NOT_INDEXED`] as a typed error, so a caller can tell "nothing to
/// read" from a real failure and answer with the message instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocsNotIndexed;

impl std::fmt::Display for DocsNotIndexed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(DOCS_NOT_INDEXED)
    }
}

impl std::error::Error for DocsNotIndexed {}

/// `.infigraph/docs-op.lock`. Exclusive for every operation that may create
/// or delete the document store: the doc watcher's reindex, `index-docs`,
/// `clean-docs`. Shared for every operation that needs the store to exist
/// and stay: reads. `DocStore`'s `DB_LOCK` serializes opens within one
/// process only, and `DocIndex::clean` takes no lock at all, so without this
/// a `clean-docs` in the CLI could delete the store under the daemon's
/// watcher, whose next open would bring it back.
pub const DOCS_OP_LOCK: &str = "docs-op.lock";

/// How long a docs operation or a read waits for the docs lock, and how
/// long a client waits for the daemon's `IndexDocs` answer. A first index
/// embeds every chunk, so this is `FullReindex`'s budget.
pub const DOCS_OP_WAIT: Duration = Duration::from_secs(600);

pub fn docs_store_path(root: &Path) -> PathBuf {
    root.join(".infigraph").join("docs.kuzu")
}

fn docs_op_lock_path(root: &Path) -> PathBuf {
    root.join(".infigraph").join(DOCS_OP_LOCK)
}

/// Whether `root` indexes its documents. The one question; see the module
/// doc. A value that does not parse is warned about once and reads as off.
pub fn docs_enabled(root: &Path) -> bool {
    Docs::resolve_or_default(RawDocs::default(), ConfigScope::Project(root))
        .enabled
        .0
}

/// Opted in *and* indexed: what a read needs before it opens anything.
pub fn docs_indexed(root: &Path) -> bool {
    docs_enabled(root) && docs_store_path(root).exists()
}

/// `[docs] enabled` as the project's own `config.toml` records it, ignoring
/// env and the user layer: `None` when it records nothing (or something
/// that is not a boolean).
pub fn docs_enabled_recorded(root: &Path) -> Option<bool> {
    settings_file::project_setting(root, SECTION, ENABLED).and_then(|item| item.as_bool())
}

/// Record the switch for `root`. Only the three writers named in the module
/// doc call this.
pub fn set_docs_enabled(root: &Path, enabled: bool) -> Result<()> {
    settings_file::set_project_setting(root, SECTION, ENABLED, toml_edit::value(enabled))
}

/// Take the docs lock exclusively, waiting up to `timeout`.
pub fn lock_docs_op(root: &Path, timeout: Duration) -> Result<crate::lockfile::LockFile> {
    crate::lockfile::acquire(&docs_op_lock_path(root), "docs-op", timeout)
}

/// Take the docs lock exclusively if it is free: `None` when another
/// operation or a reader holds it. For the doc watcher, which must never
/// block a stop behind it.
pub fn try_lock_docs_op(root: &Path) -> Result<Option<crate::lockfile::LockFile>> {
    crate::lockfile::try_acquire(&docs_op_lock_path(root), "docs-op")
}

/// Take the docs lock shared, for a read, waiting up to [`DOCS_OP_WAIT`].
/// The lock file's directory is created if missing, so a caller checks that
/// the store exists first: a read must never bring back an `.infigraph/`
/// someone removed.
pub fn lock_docs_read(root: &Path) -> Result<crate::lockfile::LockFile> {
    crate::lockfile::acquire_shared(&docs_op_lock_path(root), DOCS_OP_WAIT)
}
```

In `ps.rs`, add `crate::docs_switch::DOCS_OP_LOCK,` to `PROJECT_LOCKS` after `"docs.kuzu.lock",`, so `infigraph ps` shows a holder.

- [ ] **Step 9: Run the `docs_switch` and settings tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib docs_switch settings -- --test-threads=1`
Expected: PASS. `groups_declared_anywhere_in_the_crate_are_registered` is included and still passes, now with `docs` registered as well.

- [ ] **Step 10: Commit**

```bash
git add crates/infigraph-core/src/settings_file.rs crates/infigraph-core/src/watch/config.rs crates/infigraph-core/src/docs_switch.rs crates/infigraph-core/src/lib.rs crates/infigraph-core/src/ps.rs
git commit --no-verify -m "feat(docs): the [docs] enabled switch, the one config writer, and the docs lock" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```


---

### Task 2: Existing indexes stay on (migration at daemon start)

**Files:**
- Modify: `crates/infigraph-core/src/docs_switch.rs` (add `migrate_existing_index`, plus tests)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_daemon`, insert before `let doc_watch = …` at L590)
- Create: `crates/infigraph-cli/tests/docs_opt_in.rs`

**Interfaces:**
- Consumes: Task 1's `docs_store_path`, `set_docs_enabled`, `settings_file::project_setting`.
- Produces: `pub fn docs_switch::migrate_existing_index(root: &Path) -> anyhow::Result<bool>` (`true` when it recorded `enabled = true`). Also the CLI test harness in `docs_opt_in.rs`: `project()`, `run(root, home, backend, args)`, `start_daemon(root, home) -> Daemon`, `count(stdout, label) -> usize`, which Tasks 3, 7 and 8 extend.

- [ ] **Step 1: Write the failing unit tests.** Append to `docs_switch.rs`'s `mod tests`:

```rust
    fn with_store(root: &Path) {
        let store = docs_store_path(root);
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, b"x").unwrap();
    }

    #[test]
    fn migration_turns_an_existing_index_on() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        std::env::remove_var(ENV);
        let tmp = tempfile::tempdir().unwrap();
        with_store(tmp.path());
        assert!(migrate_existing_index(tmp.path()).unwrap());
        assert_eq!(docs_enabled_recorded(tmp.path()), Some(true));
        assert!(
            !migrate_existing_index(tmp.path()).unwrap(),
            "once is enough: a recorded value is left alone"
        );
    }

    #[test]
    fn migration_respects_an_explicit_off() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        std::env::remove_var(ENV);
        let tmp = tempfile::tempdir().unwrap();
        with_store(tmp.path());
        set_docs_enabled(tmp.path(), false).unwrap();
        assert!(!migrate_existing_index(tmp.path()).unwrap());
        assert_eq!(docs_enabled_recorded(tmp.path()), Some(false));
    }

    #[test]
    fn migration_leaves_a_project_without_an_index_alone() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        assert!(!migrate_existing_index(tmp.path()).unwrap());
        assert!(
            !settings_file::project_config_path(tmp.path()).exists(),
            "nothing to keep on, so nothing is written"
        );
    }

    /// Review Focus 2: the user's bad value is theirs to fix (doctor's
    /// settings check names it), never ours to overwrite with `true`.
    #[test]
    fn migration_leaves_a_present_but_invalid_value_alone() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        with_store(tmp.path());
        let config = settings_file::project_config_path(tmp.path());
        std::fs::write(&config, "[docs]\nenabled = \"yes\"\n").unwrap();
        assert!(!migrate_existing_index(tmp.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "[docs]\nenabled = \"yes\"\n"
        );
    }

    /// Review Focus 1: an unparseable file reads as "records nothing", but
    /// the write must refuse it rather than replace it.
    #[test]
    fn migration_leaves_an_unparseable_config_alone() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let tmp = tempfile::tempdir().unwrap();
        with_store(tmp.path());
        let config = settings_file::project_config_path(tmp.path());
        std::fs::write(&config, "[docs\nenabled = ").unwrap();
        let err = migrate_existing_index(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("invalid TOML"), "{err}");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "[docs\nenabled = ");
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib docs_switch::tests::migration -- --test-threads=1`
Expected: FAIL to compile (`migrate_existing_index` not found).

- [ ] **Step 3: Implement.** Add to `docs_switch.rs`, after `set_docs_enabled`:

```rust
/// Keep an index that predates the switch on (spec decision 2): a project
/// with a `docs.kuzu` whose `config.toml` records no `[docs] enabled` is
/// recorded as on. Any recorded value, including an explicit `false` or one
/// that does not parse as a boolean, is left alone. Returns whether it
/// recorded anything. Run once, at daemon start, before the doc thread
/// first reads the switch; this is the only migration site.
pub fn migrate_existing_index(root: &Path) -> Result<bool> {
    if settings_file::project_setting(root, SECTION, ENABLED).is_some()
        || !docs_store_path(root).exists()
    {
        return Ok(false);
    }
    set_docs_enabled(root, true)?;
    Ok(true)
}
```

For an unparseable file, `project_setting` is `None` (the loader states nothing for it), and `set_project_setting` then refuses the file with "invalid TOML", which is what the test pins.

- [ ] **Step 4: Run the unit tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib docs_switch -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Write the failing CLI test and its harness.** Create `crates/infigraph-cli/tests/docs_opt_in.rs`:

```rust
//! Document indexing is opt-in per project
//! (docs/superpowers/specs/2026-09-29-docs-opt-in-design.md), end to end
//! against a real `infigraph daemon`.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use infigraph_core::docs_switch::{docs_enabled_recorded, docs_store_path};

const DAEMON: &str = "daemon";

/// Kills and reaps the daemon on every exit path, panics included.
struct Daemon(std::process::Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli() -> &'static str {
    env!("CARGO_BIN_EXE_infigraph")
}

/// A project with one source file and one document, and a scratch `HOME`
/// so neither the registry nor a developer's `~/.infigraph/config.toml`
/// leaks in.
fn project() -> (tempfile::TempDir, tempfile::TempDir) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("hello.py"), "def hello():\n    pass\n").unwrap();
    std::fs::write(
        project.path().join("README.md"),
        "# Hello\n\nThe zebra-crossing handbook.\n",
    )
    .unwrap();
    (project, home)
}

/// Run one CLI command. `INFIGRAPH_NO_WATCH` keeps the pre-dispatch
/// auto-watch from starting a daemon the test did not ask for.
fn run(root: &Path, home: &Path, backend: &str, args: &[&str]) -> Output {
    Command::new(cli())
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env(infigraph_core::BACKEND_ENV, backend)
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_DOCS_ENABLED")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed:\nstdout={}\nstderr={}",
        stdout(out),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The number after `label` on its line of a report (`Files indexed: 1`).
fn count(text: &str, label: &str) -> usize {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(label))
        .and_then(|rest| rest.trim().parse().ok())
        .unwrap_or_else(|| panic!("no `{label}` line in:\n{text}"))
}

/// Index the code locally, then start a real daemon with a fast doc poll
/// and wait until it holds `watch.lock`.
fn start_daemon(root: &Path, home: &Path) -> Daemon {
    let bootstrap = run(root, home, infigraph_core::LOCAL_BACKEND, &["index", "--no-embed"]);
    assert_ok(&bootstrap, "bootstrap index");
    let daemon = Daemon(
        Command::new(cli())
            .args(["daemon", "--debounce", "50"])
            .current_dir(root)
            .env("HOME", home)
            .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
            .env("INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS", "50")
            .env_remove("INFIGRAPH_DOCS_ENABLED")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    assert!(
        infigraph_core::daemon::lifecycle::wait_for_daemon_ready(
            &root.join(".infigraph").join("watch.lock"),
            Duration::from_secs(30)
        ),
        "the daemon never took watch.lock"
    );
    daemon
}

/// Poll `check` for up to `budget`.
fn eventually(budget: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check()
}

/// Spec decision 2: a project that already has a document index keeps it
/// on. The daemon records the switch at start.
#[test]
fn an_existing_index_is_kept_on_when_the_daemon_starts() {
    let (project, home) = project();
    let root = project.path();
    // A real store, created the way every pre-opt-in project got one. The
    // test process runs with INFIGRAPH_BACKEND=kuzu pinned by the command.
    infigraph_docs::DocIndex::open(root).unwrap().init().unwrap();
    assert_eq!(docs_enabled_recorded(root), None);

    let _daemon = start_daemon(root, home.path());
    assert!(
        eventually(Duration::from_secs(10), || docs_enabled_recorded(root) == Some(true)),
        "the daemon must record [docs] enabled = true for an existing index"
    );
}
```

- [ ] **Step 6: Run it to see it fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test docs_opt_in -- --test-threads=1`
Expected: FAIL, "the daemon must record [docs] enabled = true for an existing index" (nothing calls the migration yet).

- [ ] **Step 7: Call the migration at daemon start.** In `cmd_daemon`, immediately before `let doc_watch = std::sync::Arc::new(…` (L590), insert:

```rust
    // Existing document indexes stay on (docs opt-in, decision 2): record
    // `[docs] enabled = true` for a project that has a `docs.kuzu` and no
    // recorded choice, before the doc thread first reads the switch. Never
    // fatal: the graph is the daemon's job.
    match infigraph_core::docs_switch::migrate_existing_index(root) {
        Ok(true) => eprintln!(
            "[daemon-start] documents: kept the existing index on ([docs] enabled = true)"
        ),
        Ok(false) => {}
        Err(e) => eprintln!("[daemon-start] documents: could not record [docs] enabled: {e:#}"),
    }
```

- [ ] **Step 8: Run the CLI test**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test docs_opt_in -- --test-threads=1`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/infigraph-core/src/docs_switch.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-cli/tests/docs_opt_in.rs
git commit --no-verify -m "feat(docs): a project with an existing index is recorded as opted in at daemon start" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```


---

### Task 3: A read creates nothing

**Files:**
- Modify: `crates/infigraph-docs/src/store.rs` (add `DocStore::open_for_read` and `DocStoreRead` after `DocStore::open`, L95-148)
- Modify: `crates/infigraph-docs/src/daemon_source.rs:15-46`
- Modify: `crates/infigraph-docs/src/daemon_store.rs:38-57` (`with_reader`)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_daemon`'s `docs_reads` comment, L737-751)
- Test: `crates/infigraph-docs/tests/docs_reads_via_daemon.rs`, `crates/infigraph-cli/tests/docs_opt_in.rs`

**Interfaces:**
- Consumes: Task 1's `docs_store_path`, `lock_docs_read`, `DocsNotIndexed`.
- Produces:
  - `pub fn DocStore::open_for_read(root: &Path) -> anyhow::Result<DocStoreRead>` (`Err(DocsNotIndexed)` when the store is absent)
  - `pub struct DocStoreRead` with `Deref<Target = DocStore>` (drops the store before the shared lock)
  - `daemon_row_source(root)` is unchanged in signature and now always registers a source.

- [ ] **Step 1: Write the failing read-service tests.** Append to `docs_reads_via_daemon.rs` (before the helpers section):

```rust
/// Documents are opt-in: the row source registers without a store, answers
/// every read with the not-indexed message, and creates nothing.
#[test]
fn a_project_without_a_store_is_answered_not_indexed_and_nothing_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let docs = infigraph_docs::daemon_source::daemon_row_source(root).unwrap();
    let svc = ReadService::start_with_sources(root, graph_source(root), Some(docs), 2).unwrap();

    let err = RemoteExec::for_docs(root)
        .query_rows("MATCH (d:Document) RETURN d.id")
        .expect_err("no store, no rows");
    assert!(
        err.to_string()
            .contains(infigraph_core::docs_switch::DOCS_NOT_INDEXED),
        "{err}"
    );
    assert!(
        !infigraph_core::docs_switch::docs_store_path(root).exists(),
        "a read must never create the store"
    );

    svc.shutdown();
}

/// Review Focus 4: the first `index-docs` creates the store while the daemon
/// runs, and the daemon serves it without a restart.
#[test]
fn a_store_created_after_the_source_is_served() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let docs = infigraph_docs::daemon_source::daemon_row_source(root).unwrap();
    let svc = ReadService::start_with_sources(root, graph_source(root), Some(docs), 2).unwrap();
    seed_one_document(root);

    let rows = RemoteExec::for_docs(root)
        .query_rows("MATCH (d:Document) RETURN d.id")
        .unwrap();
    assert_eq!(rows, vec![vec!["a.md".to_string()]]);

    svc.shutdown();
}

/// Review Focus 4: a daemon that outlived its worktree once brought a
/// removed `.infigraph/` back. A read of a project with none creates none.
#[test]
fn a_read_of_a_project_with_no_infigraph_dir_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let source = infigraph_docs::daemon_source::daemon_row_source(root).unwrap();
    let err = source("MATCH (d:Document) RETURN d.id").expect_err("nothing to read");
    assert!(
        err.is::<infigraph_core::docs_switch::DocsNotIndexed>(),
        "{err}"
    );
    assert!(!root.join(".infigraph").exists());
}

/// The explicit direct-read hatch obeys the same rule.
#[test]
fn a_direct_read_of_a_project_without_a_store_creates_nothing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("INFIGRAPH_DIRECT_READS", "1");
    let got = infigraph_docs::backend::DocBackend::get_doc_hashes(
        &infigraph_docs::daemon_store::DaemonDocStore::new(dir.path()),
    );
    std::env::remove_var("INFIGRAPH_DIRECT_READS");
    let err = got.expect_err("no store, no rows");
    assert!(
        err.is::<infigraph_core::docs_switch::DocsNotIndexed>(),
        "{err}"
    );
    assert!(!dir.path().join(".infigraph").exists());
}
```

The `RowSource` alias is `Arc<dyn Fn(&str) -> …>`, so `source("…")` calls it directly.

- [ ] **Step 2: Write the failing CLI test.** Append to `crates/infigraph-cli/tests/docs_opt_in.rs`:

```rust
/// The side effect the spec exists for: a daemon on a fresh project creates
/// no document index, across many doc-thread polls.
#[test]
fn a_fresh_daemon_creates_no_document_index() {
    let (project, home) = project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());
    // 20 doc-thread polls at 50ms.
    std::thread::sleep(Duration::from_secs(1));
    assert!(!docs_store_path(root).exists(), "the daemon created docs.kuzu");
    assert_eq!(docs_enabled_recorded(root), None);
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test docs_reads_via_daemon -- --test-threads=1`
Expected: FAIL. `a_project_without_a_store…` fails because the probe created `docs.kuzu`. `a_read_of_a_project_with_no_infigraph_dir…` fails because `.infigraph` was created. `a_direct_read…` fails because it got `Ok` with empty rows.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test docs_opt_in a_fresh_daemon -- --test-threads=1`
Expected: FAIL, "the daemon created docs.kuzu".

- [ ] **Step 4: Implement `open_for_read`.** In `store.rs`, after `impl DocStore { pub fn open … }`'s closing of `open` (L148), inside the same `impl DocStore` block, add:

```rust
    /// Open `root`'s document store to read it, never creating one.
    ///
    /// `open` creates a store that does not exist, which for a read would
    /// opt the project back in. So this checks first, and checks again
    /// under the shared docs lock: `clean-docs` takes that lock exclusively
    /// to delete, so the store cannot vanish between the check and the open.
    /// The first check also keeps the lock from creating an `.infigraph/`
    /// that is gone.
    pub fn open_for_read(root: &Path) -> Result<DocStoreRead> {
        use infigraph_core::docs_switch::{docs_store_path, lock_docs_read, DocsNotIndexed};
        let path = docs_store_path(root);
        if !path.exists() {
            return Err(DocsNotIndexed.into());
        }
        let lock = lock_docs_read(root)?;
        if !path.exists() {
            return Err(DocsNotIndexed.into());
        }
        Ok(DocStoreRead {
            store: DocStore::open(&path)?,
            _lock: lock,
        })
    }
```

After the `impl DocStore` block, add:

```rust
/// A `DocStore` opened for reading, holding the shared docs lock. Fields
/// drop in order, so the store closes before the lock is released.
pub struct DocStoreRead {
    store: DocStore,
    _lock: infigraph_core::lockfile::LockFile,
}

impl std::ops::Deref for DocStoreRead {
    type Target = DocStore;
    fn deref(&self) -> &DocStore {
        &self.store
    }
}
```

`store.rs` already imports `std::path::Path` and `anyhow::Result`. If either is missing, add it.

- [ ] **Step 5: Route the row source and the direct reads through it.** Replace `daemon_row_source` in `daemon_source.rs` (keep the doc comment and add the paragraph below to it):

```rust
/// A missing store is not a failure: documents are opt-in, and opening one
/// would create it. So the source always registers, and each request checks
/// for the store, because `index-docs` creates it while the daemon runs.
pub fn daemon_row_source(root: &Path) -> Result<infigraph_core::daemon::read_service::RowSource> {
    // Fail fast on a store that exists but cannot be opened: the daemon
    // logs this once and serves the graph only.
    match DocStore::open_for_read(root) {
        Ok(store) => drop(store),
        Err(e) if e.is::<infigraph_core::docs_switch::DocsNotIndexed>() => {}
        Err(e) => return Err(e),
    }

    let root = root.to_path_buf();
    Ok(Arc::new(move |cypher: &str| {
        let store = DocStore::open_for_read(&root)?;
        let conn = store.connection()?;
        // The guard runs here, where the connection is, so the verdict
        // still comes from the database's own parser.
        infigraph_core::daemon::read_guard::ensure_read_only(&conn, cypher)?;
        DocQuery::new(&conn).raw_query(cypher)
    }))
}
```

In `daemon_store.rs`'s `with_reader`, replace
`let store = DocStore::open(&self.root.join(".infigraph").join("docs.kuzu"))?;`
with
`let store = DocStore::open_for_read(&self.root)?;`

In `cmd_daemon` (info_commands.rs L737-741), replace the comment above `let docs_reads` with:

```rust
    // The document half of the read service. Opened here because
    // `infigraph-core` cannot name `DocStore`. It registers whether or not
    // the project has documents (they are opt-in; a missing store answers
    // "not indexed"). Only a store that exists but will not open fails it,
    // and then the daemon still serves the code graph.
```

- [ ] **Step 6: Run the tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test docs_reads_via_daemon -- --test-threads=1`
Expected: PASS, every existing test in the file included (each seeds a store first).

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test docs_opt_in -- --test-threads=1`
Expected: PASS for `a_fresh_daemon_creates_no_document_index` and `an_existing_index_is_kept_on_when_the_daemon_starts`.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-docs/src/store.rs crates/infigraph-docs/src/daemon_source.rs crates/infigraph-docs/src/daemon_store.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-docs/tests/docs_reads_via_daemon.rs crates/infigraph-cli/tests/docs_opt_in.rs
git commit --no-verify -m "fix(docs): a document read never creates docs.kuzu" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```


---

### Task 4: The executor and `clean_docs`

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs` (add `DocIndexStats` directly above `pub enum WriteResult`, L156)
- Create: `crates/infigraph-docs/src/ops.rs`
- Modify: `crates/infigraph-docs/src/lib.rs:1-13` (`pub mod ops;`)
- Create: `crates/infigraph-docs/tests/docs_ops.rs`

**Interfaces:**
- Consumes: Task 1's `set_docs_enabled`, `lock_docs_op`, `DOCS_OP_WAIT`, `docs_store_path`, `docs_enabled_recorded`. Also `DocIndex::{open, init, index, reindex, clean, store}`, `watch::ReindexGuard` (`pub(crate)`), and `DocStoreStats { document_count: usize, chunk_count: usize }`.
- Produces:
  - `#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)] pub struct daemon_protocol::DocIndexStats { pub files_scanned: usize, pub files_indexed: usize, pub chunks_created: usize, pub bfs_discovered: usize, pub documents_in_store: usize, pub chunks_in_store: usize }`
  - `pub fn infigraph_docs::ops::index_docs(root: &Path, full: bool) -> anyhow::Result<DocIndexStats>`
  - `pub fn infigraph_docs::ops::clean_docs(root: &Path) -> anyhow::Result<()>`

- [ ] **Step 1: Write the failing tests.** Create `crates/infigraph-docs/tests/docs_ops.rs`:

```rust
//! The docs executor and `clean_docs` (docs opt-in).

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use infigraph_core::docs_switch::{docs_enabled_recorded, docs_store_path, lock_docs_op};
use infigraph_docs::ops::{clean_docs, index_docs};

static ENV_LOCK: Mutex<()> = Mutex::new(());

const POLL_MS_VAR: &str = "INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS";

/// Holds `ENV_LOCK`, points `HOME` at an empty directory (so a developer's
/// `~/.infigraph/config.toml` cannot turn docs on), and clears the docs
/// variables. Restores all of it on drop, panics included.
struct Isolated {
    _lock: MutexGuard<'static, ()>,
    _home: tempfile::TempDir,
    orig_home: Option<std::ffi::OsString>,
}

impl Isolated {
    fn new() -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        std::env::remove_var("INFIGRAPH_DOCS_ENABLED");
        Self {
            _lock: lock,
            _home: home,
            orig_home,
        }
    }
}

impl Drop for Isolated {
    fn drop(&mut self) {
        std::env::remove_var(POLL_MS_VAR);
        match &self.orig_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn project_with_readme() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("README.md"), "# Hello\n\nThe zebra-crossing handbook.\n").unwrap();
    (tmp, root)
}

#[test]
fn index_docs_turns_docs_on_and_reports_real_counts() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();

    let stats = index_docs(&root, false).unwrap();

    assert_eq!(stats.files_indexed, 1, "{stats:?}");
    assert!(stats.files_scanned >= 1, "{stats:?}");
    assert!(stats.chunks_created >= 1, "{stats:?}");
    assert_eq!(stats.documents_in_store, 1, "{stats:?}");
    assert!(stats.chunks_in_store >= 1, "{stats:?}");
    assert_eq!(docs_enabled_recorded(&root), Some(true));
    assert!(docs_store_path(&root).exists());
}

#[test]
fn a_full_index_rebuilds_where_an_incremental_one_skips() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    index_docs(&root, false).unwrap();

    assert_eq!(index_docs(&root, false).unwrap().files_indexed, 0);
    assert_eq!(index_docs(&root, true).unwrap().files_indexed, 1);
}

#[test]
fn clean_docs_turns_docs_off_and_removes_the_store() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    index_docs(&root, false).unwrap();

    clean_docs(&root).unwrap();

    assert_eq!(docs_enabled_recorded(&root), Some(false));
    assert!(!docs_store_path(&root).exists());
    assert!(!root.join(".infigraph").join("docs_embeddings.bin").exists());
}

/// Review Focus 3: a docs operation already holding the lock (a watcher's
/// reindex, a `clean-docs`) runs to the end before `index_docs` touches the
/// store.
#[test]
fn index_docs_waits_for_a_docs_operation_already_running() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    let held = lock_docs_op(&root, Duration::from_secs(1)).unwrap();

    let r = root.clone();
    let indexing = std::thread::spawn(move || index_docs(&r, false));
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !indexing.is_finished(),
        "index_docs ran alongside another docs operation"
    );
    assert!(
        !docs_store_path(&root).exists(),
        "index_docs touched the store while it waited"
    );

    drop(held);
    assert_eq!(indexing.join().unwrap().unwrap().files_indexed, 1);
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test docs_ops -- --test-threads=1`
Expected: FAIL to compile (`infigraph_docs::ops` not found).

- [ ] **Step 3: Add `DocIndexStats`.** In `daemon_protocol.rs`, directly above `pub enum WriteResult` (and above its doc comment), add:

```rust
/// What one document index run did, for `index-docs` to print: the counts
/// `DocIndex::index` returns plus the store's totals afterwards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocIndexStats {
    pub files_scanned: usize,
    pub files_indexed: usize,
    pub chunks_created: usize,
    pub bfs_discovered: usize,
    pub documents_in_store: usize,
    pub chunks_in_store: usize,
}
```

- [ ] **Step 4: Write the executor.** Create `crates/infigraph-docs/src/ops.rs`:

```rust
//! Document indexing as operations (docs opt-in): the one executor,
//! `index_docs`, and `clean_docs`. With the doc watcher, these are the only
//! code that creates or deletes a project's document store, and all three
//! take the docs lock (`docs_switch::DOCS_OP_LOCK`) exclusively to do it.

use std::path::Path;

use anyhow::{Context, Result};
use infigraph_core::daemon_protocol::DocIndexStats;
use infigraph_core::docs_switch;

use crate::DocIndex;

/// The executor: records `[docs] enabled = true`, then indexes against the
/// local store in this process. `full` wipes and rebuilds (`reindex-docs`).
/// The daemon runs it for `WriteRequest::IndexDocs`. The CLI runs it
/// directly only when the process opted out of the daemon
/// (`INFIGRAPH_BACKEND=kuzu`); under the daemon backend `DocIndex::init`
/// would route, and a routed store cannot write.
pub fn index_docs(root: &Path, full: bool) -> Result<DocIndexStats> {
    docs_switch::set_docs_enabled(root, true)?;
    let _op = docs_switch::lock_docs_op(root, docs_switch::DOCS_OP_WAIT)?;
    // Work in flight for the daemon's idle exit (#203), like a watcher's
    // reindex.
    let _busy = crate::watch::ReindexGuard::enter();
    let mut idx = DocIndex::open(root)?;
    let result = if full {
        idx.reindex()?
    } else {
        idx.init()?;
        idx.index()?
    };
    let totals = idx
        .store()
        .context("doc store not initialized")?
        .stats()?;
    Ok(DocIndexStats {
        files_scanned: result.total_files,
        files_indexed: result.indexed_files,
        chunks_created: result.total_chunks,
        bfs_discovered: result.bfs_discovered,
        documents_in_store: totals.document_count,
        chunks_in_store: totals.chunk_count,
    })
}

/// `clean-docs`: turns the switch off *first*, so a watcher stops wanting
/// to write, then deletes the index under the docs lock. That order is what
/// keeps the project out: any reindex that gets the lock after this one
/// re-reads the switch and finds it off.
pub fn clean_docs(root: &Path) -> Result<()> {
    docs_switch::set_docs_enabled(root, false)?;
    let _op = docs_switch::lock_docs_op(root, docs_switch::DOCS_OP_WAIT)?;
    DocIndex::open(root)?.clean()
}
```

In `crates/infigraph-docs/src/lib.rs`, add `pub mod ops;` between `pub mod neo4j_store;` (with its `cfg`) and `pub mod query;`.

- [ ] **Step 5: Run the tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test docs_ops -- --test-threads=1`
Expected: PASS, all four.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-docs/src/ops.rs crates/infigraph-docs/src/lib.rs crates/infigraph-docs/tests/docs_ops.rs
git commit --no-verify -m "feat(docs): one executor for document indexing, and clean_docs, under the docs lock" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```


---

### Task 5: `IndexDocs`: the daemon indexes

**Files:**
- Modify: `crates/infigraph-core/src/daemon_protocol.rs` (`WriteRequest` L15-125, `WriteResult` L159-184, `serve_write`'s `FullReindex` arm L648-650, `a_write_requests_kind_is_its_variant_name` L328-340)
- Modify: `crates/infigraph-core/src/daemon/mod.rs`: `DocsHandle` (L298-304). Coordinator state (after L878). `work_in_flight!` (L946-956). Tick reap (after the SCIP-import reap that starts at L1404). `route_write` call and match (L1610-1633). Shutdown (after L1871-1882). `PendingWork` (L2328-2331). `try_start_scip_import` (L2592) and `finish_scip_import` (L2689), for placement. `route_write` (L3216-3414). `Router` (L3664-3727).
- Modify: `crates/infigraph-core/tests/daemon_control.rs:197-210` (`Recorder`)
- Create: `crates/infigraph-core/tests/daemon_index_docs.rs`
- Modify: `crates/infigraph-docs/src/ops.rs` (add `request_index_docs`, `request_index_docs_if_enabled`)
- Modify: `crates/infigraph-docs/src/daemon_store.rs:59-70` (`writes_not_routed` message)
- Modify: `crates/infigraph-cli/src/info_commands.rs:792-816` (`DocWatchHandle`)

**Interfaces:**
- Consumes: Task 4's `DocIndexStats` and `ops::index_docs`, and Task 1's `DOCS_OP_WAIT`. From #204: `WriteReply::{channel, send}`, `Task::spawn_blocking(parent: &CancellationToken, role: &'static str, f: impl FnOnce(CancellationToken) -> T + Send + 'static)`, `Task::{is_finished, join}`, `writes::{submit, WriteOpts}`, `lifecycle::ensure_daemon_for_routed_access`.
- Produces:
  - `WriteRequest::IndexDocs { full: bool }` (serde `{"IndexDocs":{"full":false}}`, `kind()` = `"IndexDocs"`)
  - `WriteResult::DocsIndexed(DocIndexStats)`
  - `DocsHandle::index_docs(&self, full: bool) -> std::result::Result<DocIndexStats, String>` (required, no default)
  - `pub const daemon::NO_DOCS_INDEXER: &str`
  - `fn try_start_docs_index(docs: Option<&Arc<dyn DocsHandle>>, full: bool, reply: WriteReply, in_flight: bool, drain_rt: &tokio::runtime::Runtime, daemon_token: &CancellationToken) -> std::result::Result<Option<PendingDocsIndex>, WriteReply>`
  - `fn finish_docs_index(reply: WriteReply, joined: std::result::Result<std::result::Result<DocIndexStats, String>, tokio::task::JoinError>)`
  - `route_write(…, scip_import_in_flight: bool, docs: Option<&Arc<dyn DocsHandle>>, docs_index_in_flight: bool) -> Routed`
  - `pub fn infigraph_docs::ops::request_index_docs(root: &Path, full: bool) -> anyhow::Result<DocIndexStats>`
  - `pub fn infigraph_docs::ops::request_index_docs_if_enabled(root: &Path) -> anyhow::Result<Option<DocIndexStats>>`

- [ ] **Step 1: Write the failing unit tests.** In `daemon/mod.rs`'s `mod tests`, extend `Router`. Add two fields to the struct:

```rust
        docs: Option<Arc<dyn DocsHandle>>,
        docs_index: Option<PendingDocsIndex>,
```

In `Router::new`, initialize them to `docs: None, docs_index: None`. Add a constructor:

```rust
        fn with_docs(root: &Path, docs: Arc<dyn DocsHandle>) -> Self {
            let mut router = Self::new(root);
            router.docs = Some(docs);
            router
        }
```

In `Router::route`, pass the two new arguments after the final `false,` (`scip_import_in_flight`):

```rust
                self.docs.as_ref(),
                self.docs_index.is_some(),
```

and add an arm before `other => other`:

```rust
                Routed::Started(PendingWork::DocsIndex(p)) => {
                    self.docs_index = Some(p);
                    Routed::Done
                }
```

Add the double and the tests:

```rust
    /// A docs handle whose `index_docs` runs `self.0`.
    struct FakeDocs(
        fn(bool) -> std::result::Result<crate::daemon_protocol::DocIndexStats, String>,
    );

    impl DocsHandle for FakeDocs {
        fn control(&self, _action: WatchAction) -> std::result::Result<(), String> {
            Ok(())
        }
        fn is_running(&self) -> bool {
            false
        }
        fn is_busy(&self) -> bool {
            false
        }
        fn index_docs(
            &self,
            full: bool,
        ) -> std::result::Result<crate::daemon_protocol::DocIndexStats, String> {
            (self.0)(full)
        }
    }

    #[test]
    fn index_docs_without_a_docs_handle_is_refused_with_a_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let mut router = Router::new(tmp.path());
        let (routed, rx) = router.route_new(WriteRequest::IndexDocs { full: false });
        assert!(matches!(routed, Routed::Done));
        assert_eq!(
            rx.recv().unwrap(),
            WriteResult::Err {
                message: NO_DOCS_INDEXER.to_string()
            }
        );
    }

    #[test]
    fn index_docs_runs_on_the_docs_handle_and_answers_with_its_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let mut router = Router::with_docs(
            tmp.path(),
            Arc::new(FakeDocs(|full| {
                Ok(crate::daemon_protocol::DocIndexStats {
                    files_indexed: if full { 7 } else { 1 },
                    ..Default::default()
                })
            })),
        );
        let (routed, rx) = router.route_new(WriteRequest::IndexDocs { full: true });
        assert!(matches!(routed, Routed::Done));
        let running = router
            .docs_index
            .take()
            .expect("started on a background task, not served inline");
        finish_docs_index(running.reply, router.drain_rt.block_on(running.task.join()));
        assert_eq!(
            rx.recv().unwrap(),
            WriteResult::DocsIndexed(crate::daemon_protocol::DocIndexStats {
                files_indexed: 7,
                ..Default::default()
            })
        );
    }

    /// Review Focus 5: one docs index at a time; a second waits in
    /// `deferred` rather than running alongside.
    #[test]
    fn a_second_index_docs_waits_for_the_running_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mut router = Router::with_docs(
            tmp.path(),
            Arc::new(FakeDocs(|_| {
                std::thread::sleep(std::time::Duration::from_millis(200));
                Ok(Default::default())
            })),
        );
        let (_, _first_rx) = router.route_new(WriteRequest::IndexDocs { full: false });
        assert!(router.docs_index.is_some());
        let (routed, _second_rx) = router.route_new(WriteRequest::IndexDocs { full: false });
        assert!(
            matches!(
                routed,
                Routed::NotYet(WriteRequest::IndexDocs { full: false }, _)
            ),
            "a second docs index must wait, not run alongside"
        );
        let running = router.docs_index.take().unwrap();
        finish_docs_index(running.reply, router.drain_rt.block_on(running.task.join()));
    }

    /// Review Focus 5: a panicking executor still answers its client.
    #[test]
    fn a_panicking_docs_index_still_answers_its_client() {
        let tmp = tempfile::tempdir().unwrap();
        let mut router =
            Router::with_docs(tmp.path(), Arc::new(FakeDocs(|_| panic!("indexer blew up"))));
        let (_, rx) = router.route_new(WriteRequest::IndexDocs { full: false });
        let running = router.docs_index.take().unwrap();
        finish_docs_index(running.reply, router.drain_rt.block_on(running.task.join()));
        match rx.recv().unwrap() {
            WriteResult::Err { message } => {
                assert!(message.contains("document indexing"), "{message}")
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }
```

In `daemon_protocol.rs`'s `a_write_requests_kind_is_its_variant_name`, add:

```rust
        assert_eq!(WriteRequest::IndexDocs { full: true }.kind(), "IndexDocs");
        assert_eq!(
            serde_json::to_string(&WriteRequest::IndexDocs { full: false }).unwrap(),
            r#"{"IndexDocs":{"full":false}}"#
        );
```

- [ ] **Step 2: Write the failing integration test.** Create `crates/infigraph-core/tests/daemon_index_docs.rs`:

```rust
//! `WriteRequest::IndexDocs` over the socket to a real write coordinator
//! (docs opt-in). The executor lives in `infigraph-docs`; the coordinator
//! reaches it through the `DocsHandle` the daemon's owner supplies, so a
//! canned handle stands in for it here.

mod common;
use common::daemon::{start, start_with_docs, stop, ENV_LOCK};

use std::sync::Arc;
use std::time::{Duration, Instant};

use infigraph_core::daemon::read_protocol::WatchAction;
use infigraph_core::daemon::writes::{submit, WriteOpts};
use infigraph_core::daemon_protocol::{DocIndexStats, WriteRequest, WriteResult};

/// Answers `index_docs` after `delay` with a count that shows `full`.
struct CannedIndexer {
    delay: Duration,
}

impl infigraph_core::daemon::DocsHandle for CannedIndexer {
    fn control(&self, _action: WatchAction) -> Result<(), String> {
        Ok(())
    }
    fn is_running(&self) -> bool {
        false
    }
    fn is_busy(&self) -> bool {
        false
    }
    fn index_docs(&self, full: bool) -> Result<DocIndexStats, String> {
        std::thread::sleep(self.delay);
        Ok(DocIndexStats {
            files_indexed: if full { 2 } else { 1 },
            ..Default::default()
        })
    }
}

fn opts() -> WriteOpts<'static> {
    WriteOpts {
        timeout: Duration::from_secs(120),
        cancel: None,
    }
}

#[test]
fn index_docs_is_answered_with_the_indexers_stats() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start_with_docs(
        dir.path(),
        Some(Arc::new(CannedIndexer {
            delay: Duration::from_millis(100),
        })),
    );
    let reply = submit(dir.path(), &WriteRequest::IndexDocs { full: true }, opts()).unwrap();
    assert_eq!(
        reply,
        WriteResult::DocsIndexed(DocIndexStats {
            files_indexed: 2,
            ..Default::default()
        })
    );
    stop(d);
}

#[test]
fn a_daemon_without_a_docs_handle_refuses_index_docs_with_a_reason() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start(dir.path());
    let reply = submit(dir.path(), &WriteRequest::IndexDocs { full: false }, opts()).unwrap();
    assert_eq!(
        reply,
        WriteResult::Err {
            message: infigraph_core::daemon::NO_DOCS_INDEXER.to_string()
        }
    );
    stop(d);
}

/// Spec: a long first index never blocks code-graph writes. It runs on a
/// background task, not the coordinator thread.
#[test]
fn code_writes_are_served_while_documents_index() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let d = start_with_docs(
        dir.path(),
        Some(Arc::new(CannedIndexer {
            delay: Duration::from_secs(5),
        })),
    );
    // Warm: the graph is open before anything is timed.
    submit(dir.path(), &WriteRequest::Index { paths: None }, opts()).unwrap();

    let root = dir.path().to_path_buf();
    let indexing =
        std::thread::spawn(move || submit(&root, &WriteRequest::IndexDocs { full: false }, opts()));
    std::thread::sleep(Duration::from_millis(300));

    let t = Instant::now();
    let code = submit(
        dir.path(),
        &WriteRequest::UpsertRepo {
            namespace: "n".into(),
        },
        opts(),
    )
    .unwrap();
    assert!(matches!(code, WriteResult::Ok { .. }), "{code:?}");
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "a code write waited {:?} behind a docs index",
        t.elapsed()
    );
    assert!(matches!(
        indexing.join().unwrap().unwrap(),
        WriteResult::DocsIndexed(_)
    ));
    stop(d);
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::tests daemon_protocol::tests -- --test-threads=1`
Expected: FAIL to compile (`WriteRequest::IndexDocs`, `PendingDocsIndex` and `NO_DOCS_INDEXER` not found).

- [ ] **Step 4: The wire types.** In `daemon_protocol.rs`, add as the last `WriteRequest` variant, after `FullReindex,`:

```rust
    /// Index this project's documents (docs opt-in). The daemon runs the
    /// executor (`infigraph_docs::ops::index_docs`) through its
    /// `DocsHandle` on a background task. It records `[docs] enabled =
    /// true`, so it is also how a project opts in. `full` wipes and
    /// rebuilds (`reindex-docs`). There is no document data on the socket:
    /// the daemon reads the files itself.
    IndexDocs { full: bool },
```

Add as the last `WriteResult` variant, before `Err { … }`:

```rust
    /// `IndexDocs`'s outcome. A variant of its own: `Ok`'s two fields
    /// describe code-graph writes.
    DocsIndexed(DocIndexStats),
```

In `serve_write`, after the `WriteRequest::FullReindex => …` arm, add:

```rust
        WriteRequest::IndexDocs { .. } => WriteResult::Err {
            message: "IndexDocs runs on the daemon's docs handle, never under index.lock"
                .to_string(),
        },
```

- [ ] **Step 5: The trait method and every implementer.** In `daemon/mod.rs`, add to `trait DocsHandle` after `is_busy`:

```rust
    /// Index this project's documents for `WriteRequest::IndexDocs`. Called
    /// on a background task, never the coordinator thread, and blocks until
    /// done. `Err(msg)` is the reply the client gets. Required, with no
    /// default: a handle that forgot it would silently refuse every
    /// `index-docs`.
    fn index_docs(
        &self,
        full: bool,
    ) -> std::result::Result<crate::daemon_protocol::DocIndexStats, String>;
```

Directly below the trait, add:

```rust
/// The reply to `IndexDocs` from a coordinator started without a docs
/// handle: every in-process watcher, which serves no socket writes anyway.
pub const NO_DOCS_INDEXER: &str =
    "this daemon does not index documents: it was not started by `infigraph daemon`";
```

In `crates/infigraph-core/tests/daemon_control.rs`, add to `impl DocsHandle for Recorder`:

```rust
    fn index_docs(
        &self,
        _full: bool,
    ) -> Result<infigraph_core::daemon_protocol::DocIndexStats, String> {
        Err("the Recorder test double does not index".to_string())
    }
```

In `crates/infigraph-cli/src/info_commands.rs`, add to `impl infigraph_core::daemon::DocsHandle for DocWatchHandle`:

```rust
    fn index_docs(
        &self,
        full: bool,
    ) -> std::result::Result<infigraph_core::daemon_protocol::DocIndexStats, String> {
        let root = self.0.lock().unwrap().root.clone();
        infigraph_docs::ops::index_docs(&root, full).map_err(|e| format!("{e:#}"))
    }
```

The mutex guard is dropped at the end of the `let root` statement, so the doc thread's `stop`/`start` is never blocked behind an index.

- [ ] **Step 6: Start, reap and join it in the coordinator.** Next to `PendingScipImport` (L2297-2315), add:

```rust
/// A `WriteRequest::IndexDocs` running on the docs handle, and the client
/// owed its result. One at a time: a second waits in `deferred`.
struct PendingDocsIndex {
    task: Task<std::result::Result<crate::daemon_protocol::DocIndexStats, String>>,
    reply: WriteReply,
}
```

Add `DocsIndex(PendingDocsIndex),` to `enum PendingWork`, and extend its doc comment: "…or a docs index, which touches no graph and runs alongside either."

After `try_start_scip_import` (ends L2681), add:

```rust
/// Loop-thread entry point for `WriteRequest::IndexDocs`: hands it to the
/// docs handle on a background task, like a SCIP import, so a first index
/// (embeddings included, minutes on a large tree) never blocks code-graph
/// writes. It needs no `index.lock`: it writes only the document store,
/// which the docs lock guards. With no handle there is nothing to run it,
/// and the client is told why at once. `Err(reply)` means one is already
/// running: try again next tick.
fn try_start_docs_index(
    docs: Option<&Arc<dyn DocsHandle>>,
    full: bool,
    reply: WriteReply,
    in_flight: bool,
    drain_rt: &tokio::runtime::Runtime,
    daemon_token: &CancellationToken,
) -> std::result::Result<Option<PendingDocsIndex>, WriteReply> {
    let Some(docs) = docs else {
        reply.send(WriteResult::Err {
            message: NO_DOCS_INDEXER.to_string(),
        });
        return Ok(None);
    };
    if in_flight {
        return Err(reply);
    }
    let docs = Arc::clone(docs);
    let task = {
        let _guard = drain_rt.enter();
        Task::spawn_blocking(daemon_token, "docs-index", move |_token| {
            docs.index_docs(full)
        })
    };
    Ok(Some(PendingDocsIndex { task, reply }))
}

/// Answers a finished docs index. A panicked task answers too: a client
/// must never wait out its timeout for a result nobody will send.
fn finish_docs_index(
    reply: WriteReply,
    joined: std::result::Result<
        std::result::Result<crate::daemon_protocol::DocIndexStats, String>,
        tokio::task::JoinError,
    >,
) {
    let result = match joined {
        Ok(Ok(stats)) => {
            eprintln!(
                "[daemon] documents indexed: {} of {} files, {} chunks ({} documents in store)",
                stats.files_indexed,
                stats.files_scanned,
                stats.chunks_created,
                stats.documents_in_store
            );
            WriteResult::DocsIndexed(stats)
        }
        Ok(Err(message)) => WriteResult::Err { message },
        Err(e) => WriteResult::Err {
            message: format!("document indexing task failed: {e}"),
        },
    };
    reply.send(result);
}
```

In `route_write`, add two parameters after `scip_import_in_flight: bool,`:

```rust
    docs: Option<&Arc<dyn DocsHandle>>,
    docs_index_in_flight: bool,
```

Add an arm before `other => match serve_request_locked(`:

```rust
        WriteRequest::IndexDocs { full } => match try_start_docs_index(
            docs,
            full,
            reply,
            docs_index_in_flight,
            drain_rt,
            daemon_token,
        ) {
            Ok(Some(p)) => Routed::Started(PendingWork::DocsIndex(p)),
            Ok(None) => Routed::Done,
            Err(reply) => Routed::NotYet(WriteRequest::IndexDocs { full }, reply),
        },
```

and extend `route_write`'s doc comment: "…`IndexDocs` runs on the docs handle's background task (one at a time); everything else …".

In `run_write_coordinator`, directly after `let mut scip_import_in_flight: Option<PendingScipImport> = None;`, add:

```rust
    // A `WriteRequest::IndexDocs` running on `docs_control` (docs opt-in),
    // reaped by `finish_docs_index` below.
    let mut docs_index_in_flight: Option<PendingDocsIndex> = None;
```

In `work_in_flight!`, add `|| docs_index_in_flight.is_some()` after `|| scip_import_in_flight.is_some()`.

Directly after the SCIP-import reap block (the `if scip_import_in_flight.as_ref().is_some_and(…) { … }` that starts at L1404), add:

```rust
        if docs_index_in_flight
            .as_ref()
            .is_some_and(|p| p.task.is_finished())
        {
            let PendingDocsIndex { task, reply } = docs_index_in_flight
                .take()
                .expect("checked is_some just above");
            finish_docs_index(reply, drain_rt.block_on(task.join()));
        }
```

In the `deferred` loop's `route_write(…)` call, add the two arguments after `scip_import_in_flight.is_some(),`:

```rust
                    docs_control.as_ref(),
                    docs_index_in_flight.is_some(),
```

and add to its `match` after the `PendingWork::ScipImport(p)` arm:

```rust
                    Routed::Started(PendingWork::DocsIndex(p)) => docs_index_in_flight = Some(p),
```

At shutdown, directly after the `if let Some(in_flight) = scip_import_in_flight.take() { … }` block, add:

```rust
    // A docs index that started finishes and answers its client (#204 D3:
    // started work is never cancelled).
    if let Some(PendingDocsIndex { task, reply }) = docs_index_in_flight.take() {
        finish_docs_index(reply, drain_rt.block_on(task.join()));
    }
```

- [ ] **Step 7: Run the core tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib daemon::tests daemon_protocol::tests -- --test-threads=1`
Expected: PASS, including the four new router tests and every existing `route_write` test.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --test daemon_index_docs --test daemon_control --test daemon_socket_writes -- --test-threads=1`
Expected: PASS.

- [ ] **Step 8: The client entry point.** Append to `crates/infigraph-docs/src/ops.rs`:

```rust
/// How `index-docs` runs, for every caller: under the daemon backend it
/// asks the daemon (`WriteRequest::IndexDocs`), which runs [`index_docs`]
/// beside the store it owns; otherwise (`INFIGRAPH_BACKEND=kuzu`, or
/// remote) it runs [`index_docs`] here. There is no second indexer.
pub fn request_index_docs(root: &Path, full: bool) -> Result<DocIndexStats> {
    use infigraph_core::daemon_protocol::{WriteRequest, WriteResult};

    if !infigraph_core::daemon_backend_selected() {
        return index_docs(root, full);
    }
    infigraph_core::daemon::lifecycle::ensure_daemon_for_routed_access(root)?;
    let result = infigraph_core::daemon::writes::submit(
        root,
        &WriteRequest::IndexDocs { full },
        infigraph_core::daemon::writes::WriteOpts {
            timeout: docs_switch::DOCS_OP_WAIT,
            cancel: None,
        },
    )?;
    match result {
        WriteResult::DocsIndexed(stats) => Ok(stats),
        WriteResult::Err { message } => anyhow::bail!("document indexing failed: {message}"),
        other => anyhow::bail!("document indexing returned an unexpected result: {other:?}"),
    }
}

/// [`request_index_docs`], but only for a project that has opted in:
/// `None` for one that has not. A group build and MCP's in-process
/// `index_project` refresh documents this way, so neither opts a project in
/// on its owner's behalf.
pub fn request_index_docs_if_enabled(root: &Path) -> Result<Option<DocIndexStats>> {
    if !docs_switch::docs_enabled(root) {
        return Ok(None);
    }
    request_index_docs(root, false).map(Some)
}
```

Append to `crates/infigraph-docs/tests/docs_ops.rs`:

```rust
#[test]
fn request_index_docs_if_enabled_leaves_a_project_that_has_not_opted_in_alone() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    assert_eq!(
        infigraph_docs::ops::request_index_docs_if_enabled(&root).unwrap(),
        None
    );
    assert!(!root.join(".infigraph").exists());
}

#[test]
fn request_index_docs_without_the_daemon_runs_the_executor_here() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    let stats = infigraph_docs::ops::request_index_docs(&root, false).unwrap();
    assert_eq!(stats.files_indexed, 1);
    assert_eq!(docs_enabled_recorded(&root), Some(true));
}
```

In `daemon_store.rs`, replace `writes_not_routed`'s doc comment and message:

```rust
    /// A document write never travels as data (docs opt-in, approach A):
    /// the daemon indexes documents itself, through `IndexDocs` and its doc
    /// watcher. A client-side write is refused explicitly rather than
    /// silently opening `docs.kuzu` beside the daemon's own handle.
    fn writes_not_routed(method: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "document writes are not routed through the daemon ({method}); run `infigraph \
             index-docs`, which asks the daemon to index this project's documents, or set \
             INFIGRAPH_BACKEND=kuzu to write directly."
        )
    }
```

`a_routed_document_write_is_refused_with_an_explanation` still passes: it checks for "not routed through the daemon".

- [ ] **Step 9: Run the docs tests and build the CLI**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test docs_ops --test docs_reads_via_daemon -- --test-threads=1`
Expected: PASS.

Run: `cargo build -p infigraph-cli -p infigraph-mcp`
Expected: builds with no errors. `DocWatchHandle` implements the new required method.

- [ ] **Step 10: Commit**

```bash
git add crates/infigraph-core/src/daemon_protocol.rs crates/infigraph-core/src/daemon/mod.rs crates/infigraph-core/tests/daemon_control.rs crates/infigraph-core/tests/daemon_index_docs.rs crates/infigraph-docs/src/ops.rs crates/infigraph-docs/src/daemon_store.rs crates/infigraph-docs/tests/docs_ops.rs crates/infigraph-cli/src/info_commands.rs
git commit --no-verify -m "feat(daemon): WriteRequest::IndexDocs runs the docs executor through the DocsHandle" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```


---

### Task 6: `index-docs`, `reindex-docs`, `clean-docs` (CLI and MCP fallbacks)

**Files:**
- Modify: `crates/infigraph-docs/src/ops.rs` (add `stats_report`)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_index_docs` L1086-1136, `cmd_reindex_docs` L1138-1148, `cmd_clean_docs` L1150-1155)
- Modify: `crates/infigraph-cli/src/main.rs:1188-1202`
- Modify: `crates/infigraph-mcp/src/tools/docs.rs` (`tool_index_docs` fallback L200-217, `tool_clean_docs` fallback L319-324, `tool_reindex_docs` fallback L353-358)
- Test: `crates/infigraph-cli/tests/docs_opt_in.rs`

**Interfaces:**
- Consumes: Task 5's `request_index_docs`, Task 4's `clean_docs` and `DocIndexStats`.
- Produces: `pub fn infigraph_docs::ops::stats_report(what: &str, stats: &DocIndexStats, elapsed: std::time::Duration) -> String`. `pub(crate) fn cmd_index_docs(root: &Path) -> Result<()>` (the `namespace` parameter moves into the remote branch).

- [ ] **Step 1: Write the failing end-to-end tests.** Append to `crates/infigraph-cli/tests/docs_opt_in.rs`:

```rust
#[test]
fn index_docs_under_the_daemon_backend_indexes_opts_in_and_is_searchable() {
    let (project, home) = project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());

    let out = run(root, home.path(), DAEMON, &["index-docs"]);
    assert_ok(&out, "index-docs");
    let text = stdout(&out);
    assert!(count(&text, "Files indexed:") >= 1, "{text}");
    assert!(count(&text, "Total documents in store:") >= 1, "{text}");
    assert_eq!(docs_enabled_recorded(root), Some(true));

    let found = run(root, home.path(), DAEMON, &["search-docs", "zebra"]);
    assert_ok(&found, "search-docs");
    assert!(stdout(&found).contains("README.md"), "{}", stdout(&found));
}

#[test]
fn index_docs_with_the_daemon_opted_out_indexes_in_process() {
    let (project, home) = project();
    let root = project.path();
    let local = infigraph_core::LOCAL_BACKEND;

    let out = run(root, home.path(), local, &["index-docs"]);
    assert_ok(&out, "index-docs");
    assert!(count(&stdout(&out), "Files indexed:") >= 1, "{}", stdout(&out));
    assert_eq!(docs_enabled_recorded(root), Some(true));
    assert!(docs_store_path(root).exists());

    let found = run(root, home.path(), local, &["search-docs", "zebra"]);
    assert_ok(&found, "search-docs");
    assert!(stdout(&found).contains("README.md"), "{}", stdout(&found));
}

#[test]
fn reindex_docs_rebuilds_through_the_daemon() {
    let (project, home) = project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());
    assert_ok(&run(root, home.path(), DAEMON, &["index-docs"]), "index-docs");

    let again = run(root, home.path(), DAEMON, &["index-docs"]);
    assert_ok(&again, "second index-docs");
    assert_eq!(
        count(&stdout(&again), "Files indexed:"),
        0,
        "an incremental run skips unchanged files"
    );

    let full = run(root, home.path(), DAEMON, &["reindex-docs"]);
    assert_ok(&full, "reindex-docs");
    assert!(stdout(&full).contains("full reindex"), "{}", stdout(&full));
    assert!(
        count(&stdout(&full), "Files indexed:") >= 1,
        "a full rebuild re-indexes every file: {}",
        stdout(&full)
    );
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test docs_opt_in -- --test-threads=1`
Expected: FAIL. `index_docs_under_the_daemon_backend…` fails with "document writes are not routed through the daemon (upsert_docs)". `index_docs_with_the_daemon_opted_out…` fails at `docs_enabled_recorded == Some(true)`.

- [ ] **Step 3: The shared report.** Append to `crates/infigraph-docs/src/ops.rs`:

```rust
/// The report `index-docs` and `reindex-docs` print, and MCP's in-process
/// fallbacks return.
pub fn stats_report(what: &str, stats: &DocIndexStats, elapsed: std::time::Duration) -> String {
    format!(
        "{what} complete in {:.1}s\n  Files scanned: {}\n  Files indexed: {}\n  Chunks created: {}\n  Total documents in store: {}\n  Total chunks in store: {}",
        elapsed.as_secs_f64(),
        stats.files_scanned,
        stats.files_indexed,
        stats.chunks_created,
        stats.documents_in_store,
        stats.chunks_in_store
    )
}
```

- [ ] **Step 4: The CLI commands.** Replace `cmd_index_docs`, `cmd_reindex_docs` and `cmd_clean_docs` (info_commands.rs L1086-1155) with:

```rust
pub(crate) fn cmd_index_docs(root: &Path) -> Result<()> {
    #[cfg(feature = "remote")]
    if infigraph_core::daemon::lifecycle::is_remote_backend() {
        let ns = infigraph_core::multi::Registry::load()
            .ok()
            .and_then(|reg| reg.resolve_repo_namespace(root));
        return cmd_index_docs_remote(root, ns.as_deref());
    }
    let start = std::time::Instant::now();
    let stats = infigraph_docs::ops::request_index_docs(root, false)?;
    println!(
        "{}",
        infigraph_docs::ops::stats_report("Document indexing", &stats, start.elapsed())
    );
    Ok(())
}

/// Remote (Neo4j + Postgres) mode keeps its direct write path: the store is
/// a real client/server database, and embeddings go to pgvector.
#[cfg(feature = "remote")]
fn cmd_index_docs_remote(root: &Path, namespace: Option<&str>) -> Result<()> {
    let start = std::time::Instant::now();
    let mut idx = infigraph_docs::DocIndex::open(root)?;
    if let Some(ns) = namespace {
        idx.set_namespace(ns);
    }
    idx.set_skip_file_embeddings(true);
    idx.init()?;
    let result = idx.index()?;
    let elapsed = start.elapsed();
    println!(
        "Document indexing complete in {:.1}s\n  Files scanned: {}\n  Files indexed: {}\n  Chunks created: {}",
        elapsed.as_secs_f64(), result.total_files, result.indexed_files, result.total_chunks
    );
    let store = idx.store().context("doc store not initialized")?;
    let stats = store.stats()?;
    println!(
        "  Total documents in store: {}\n  Total chunks in store: {}",
        stats.document_count, stats.chunk_count
    );
    let pg = infigraph_core::meta::PostgresMetaStore::connect_from_env_cached()?;
    pg.init_schema()?;
    let chunk_refs: Vec<&infigraph_docs::chunk::Chunk> = result.new_chunks.iter().collect();
    let changed_refs: Vec<&str> = result.changed_files.iter().map(|s| s.as_str()).collect();
    let count =
        infigraph_docs::embed::update_doc_embeddings_remote(store, &pg, &chunk_refs, &changed_refs)?;
    if count > 0 {
        println!("Saved {} doc embeddings to Postgres pgvector", count);
    }
    Ok(())
}

pub(crate) fn cmd_reindex_docs(root: &Path) -> Result<()> {
    let start = std::time::Instant::now();
    let stats = infigraph_docs::ops::request_index_docs(root, true)?;
    println!(
        "{}",
        infigraph_docs::ops::stats_report("Document full reindex", &stats, start.elapsed())
    );
    Ok(())
}

pub(crate) fn cmd_clean_docs(root: &Path) -> Result<()> {
    infigraph_docs::ops::clean_docs(root)?;
    println!("Document index cleaned; document indexing is off for this project.");
    Ok(())
}
```

`cmd_index_docs_remote` is the old body's remote path, moved verbatim with `is_remote` fixed to `true`. In `main.rs`, replace the `Commands::IndexDocs => { … }` arm (L1188-1200) with:

```rust
        Commands::IndexDocs => cmd_index_docs(root),
```

- [ ] **Step 5: MCP's in-process fallbacks.** In `tool_index_docs`, replace everything after the `if let Some(cli) = find_infigraph_cli() { … }` block (L200-217) with:

```rust
    let started = std::time::Instant::now();
    let stats = infigraph_docs::ops::request_index_docs(std::path::Path::new(path), false)?;
    auto_start_doc_watch(path);
    Ok(infigraph_docs::ops::stats_report(
        "Document indexing",
        &stats,
        started.elapsed(),
    ))
```

In `tool_clean_docs`, replace the fallback after the CLI block (L319-324) with:

```rust
    infigraph_docs::ops::clean_docs(&PathBuf::from(path))?;
    Ok("Document index cleaned; document indexing is off for this project.".to_string())
```

In `tool_reindex_docs`, replace the fallback after the CLI block (L353-358) with:

```rust
    let started = std::time::Instant::now();
    let stats = infigraph_docs::ops::request_index_docs(&PathBuf::from(path), true)?;
    Ok(infigraph_docs::ops::stats_report(
        "Document full reindex",
        &stats,
        started.elapsed(),
    ))
```

- [ ] **Step 6: Run the tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test docs_opt_in -- --test-threads=1`
Expected: PASS, all five.

Run: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-mcp --test watcher_reindex -- --test-threads=1`
Expected: PASS. The executor creates the store and records the switch; MCP's auto-start still keys on the store until Task 7.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --bin infigraph -- --test-threads=1`
Expected: PASS (`should_auto_watch_allows_only_source_ingesting_commands` still matches `Commands::IndexDocs`).

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-docs/src/ops.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-cli/src/main.rs crates/infigraph-mcp/src/tools/docs.rs crates/infigraph-cli/tests/docs_opt_in.rs
git commit --no-verify -m "feat(cli): index-docs, reindex-docs and clean-docs work under the daemon backend" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```


---

### Task 7: The doc watcher follows the switch

**Files:**
- Modify: `crates/infigraph-docs/src/watch.rs` (`watch_docs` reindex block L76-118; `attach_poll_interval` doc L187-190; `watch_docs_daemon_loop` L202-236; `run_attached_cycle` L238-284; tests `chunk_count` L408-420, `does_not_attach_without_docs_kuzu` L432-451, `attaches_and_indexes_once_docs_kuzu_appears` L453-492, `a_stray_stop_docs_file_no_longer_detaches_the_loop` L543-571, `run_attached_cycle_returns_when_watch_fn_exits_unrequested` L573-601)
- Modify: `crates/infigraph-mcp/src/tools/docs.rs:77-79` (`auto_start_doc_watch_inner`)
- Modify: `crates/infigraph-mcp/tests/watcher_reindex.rs:1095-1142` (`test_doc_watch_noop_before_doc_index_then_starts_after`)
- Modify: `crates/infigraph-mcp/tests/watcher_daemon_mode.rs` (`auto_start_doc_watch_respects_daemon_mode_toggle` L441-468, `auto_start_doc_watch_respects_watch_docs_enabled_policy` L477-500)
- Test: `crates/infigraph-docs/tests/docs_ops.rs`, `crates/infigraph-cli/tests/docs_opt_in.rs` (the `clean-docs` race)
- Modify: `crates/infigraph-cli/tests/watch_daemon_docs.rs` (`cmd_watch_daemon_also_indexes_docs_without_restart` L165-184, `watch_docs_stop_and_start_over_control_are_visible_in_status` L548-555 and L621)

**Interfaces:**
- Consumes: Task 1's `docs_enabled`, `set_docs_enabled`, `try_lock_docs_op`, `docs_store_path`. Task 4's `ops::{index_docs, clean_docs}` (for the race test). Task 6's `index-docs`/`clean-docs`, which now record the switch, so MCP's auto-start and the CLI end-to-end test can key on it.
- Produces: `watch_docs_daemon_loop(root, debounce_ms, shutdown)` with the same signature, now attaching while `docs_enabled(root)`. Private `fn reindex_if_enabled(root: &Path, log_prefix: &str) -> Reindex` and `enum Reindex { Busy, Done { indexed: bool } }`. `run_attached_cycle(still_wanted: impl Fn() -> bool, shutdown, poll, watch_fn)`.

- [ ] **Step 1: Rewrite the attach tests against the switch.** In `watch.rs`'s `mod tests`:

At the top of `chunk_count`'s body, add a guard so the helper never creates the store it is measuring:

```rust
        if !infigraph_core::docs_switch::docs_store_path(root).exists() {
            return 0;
        }
```

Replace `does_not_attach_without_docs_kuzu` with:

```rust
    #[test]
    fn does_not_attach_or_create_anything_while_docs_are_off() {
        let _poll = FastPoll::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        std::fs::write(root.join("readme.md"), "# hello\n\nsome content").unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let loop_root = root.clone();
        let handle =
            std::thread::spawn(move || watch_docs_daemon_loop(&loop_root, 50, shutdown_clone));

        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !infigraph_core::docs_switch::docs_store_path(&root).exists(),
            "a watcher for a project that has not opted in must create nothing"
        );
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.join().unwrap().unwrap();
    }
```

Replace `attaches_and_indexes_once_docs_kuzu_appears` with the same body under a new name, changing only the setup line. Replace
`crate::DocIndex::open(&root).unwrap().init().unwrap();`
`assert!(root.join(".infigraph").join("docs.kuzu").exists());`
with
`infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();`
and rename the test `attaches_and_indexes_once_docs_are_enabled`. The loop now creates the store on its first catch-up. The final `assert!(chunks > 0, …)` stays as it is.

In `a_stray_stop_docs_file_no_longer_detaches_the_loop`, replace
`crate::DocIndex::open(&root).unwrap().init().unwrap();`
with
`infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();`

In `run_attached_cycle_returns_when_watch_fn_exits_unrequested`, replace the `docs_kuzu` binding and the call's first argument:

```rust
            run_attached_cycle(
                || true,
                &shutdown,
                Duration::from_millis(10),
                |_stop_rx: mpsc::Receiver<()>| -> Result<()> { Ok(()) },
            );
```

and delete `let docs_kuzu = root.join("docs.kuzu");` along with the now-unused `root` binding.

Add two tests:

```rust
    /// Turning the switch off stops the attached watcher.
    #[test]
    fn run_attached_cycle_stops_the_watcher_once_it_is_no_longer_wanted() {
        let wanted = Arc::new(AtomicBool::new(true));
        let (stopped_tx, stopped_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let still = Arc::clone(&wanted);
        let cycle = std::thread::spawn(move || {
            run_attached_cycle(
                move || still.load(Ordering::SeqCst),
                &shutdown,
                Duration::from_millis(10),
                move |stop_rx: mpsc::Receiver<()>| -> Result<()> {
                    let _ = stop_rx.recv();
                    let _ = stopped_tx.send(());
                    Ok(())
                },
            );
        });
        std::thread::sleep(Duration::from_millis(50));
        wanted.store(false, Ordering::SeqCst);
        stopped_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the watcher must be told to stop once docs are off");
        cycle.join().unwrap();
    }

    /// With the switch off, a document change indexes nothing, even for a
    /// watcher that was attached when it went off.
    #[test]
    fn a_watcher_indexes_nothing_after_docs_are_turned_off() {
        let _poll = FastPoll::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();
        std::fs::write(root.join("readme.md"), "# hello\n\nsome content").unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let loop_root = root.clone();
        let handle =
            std::thread::spawn(move || watch_docs_daemon_loop(&loop_root, 50, shutdown_clone));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while chunk_count(&root) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(chunk_count(&root) > 0, "precondition: the watcher attached");

        infigraph_core::docs_switch::set_docs_enabled(&root, false).unwrap();
        std::thread::sleep(Duration::from_millis(1000));
        let before = chunk_count(&root);
        std::fs::write(root.join("second.md"), "# second\n\nmore content").unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(chunk_count(&root), before, "a watcher indexed with docs off");

        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        handle.join().unwrap().unwrap();
    }
```

`tests` already has `use super::*;`, which brings in `Ordering`, `mpsc` and `Result`, and it has `AtomicBool`, `Arc` and `Duration` imports.

Pin the `clean-docs` race too, in process and end to end (Review Focus 3). Append to `crates/infigraph-docs/tests/docs_ops.rs`:

```rust
/// Review Focus 3: `clean-docs` while the daemon's doc watcher is attached
/// and has a change pending. The switch goes off before the delete, and the
/// watcher re-reads it under the docs lock, so nothing brings the store back.
#[test]
fn clean_docs_is_not_undone_by_an_attached_watcher() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let _env = Isolated::new();
    std::env::set_var(POLL_MS_VAR, "20");
    let (_tmp, root) = project_with_readme();
    index_docs(&root, false).unwrap();

    let shutdown = Arc::new(AtomicBool::new(false));
    let loop_root = root.clone();
    let loop_shutdown = Arc::clone(&shutdown);
    let watcher = std::thread::spawn(move || {
        infigraph_docs::watch::watch_docs_daemon_loop(&loop_root, 50, loop_shutdown)
    });
    std::thread::sleep(Duration::from_millis(500));

    std::fs::write(root.join("second.md"), "# Second\n\nmore\n").unwrap();
    clean_docs(&root).unwrap();
    std::thread::sleep(Duration::from_millis(1500));

    shutdown.store(true, Ordering::Relaxed);
    watcher.join().unwrap().unwrap();
    assert!(
        !docs_store_path(&root).exists(),
        "the watcher recreated the store clean-docs removed"
    );
    assert_eq!(docs_enabled_recorded(&root), Some(false));
}
```

Append to `crates/infigraph-cli/tests/docs_opt_in.rs`:

```rust
#[test]
fn clean_docs_turns_docs_off_and_the_daemon_does_not_bring_them_back() {
    let (project, home) = project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());
    assert_ok(&run(root, home.path(), DAEMON, &["index-docs"]), "index-docs");
    assert!(docs_store_path(root).exists());

    let out = run(root, home.path(), DAEMON, &["clean-docs"]);
    assert_ok(&out, "clean-docs");
    assert_eq!(docs_enabled_recorded(root), Some(false));
    assert!(!docs_store_path(root).exists());

    // A new document, then many doc-thread polls: nothing comes back.
    std::fs::write(root.join("NEW.md"), "# New\n\nanother page\n").unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !docs_store_path(root).exists(),
        "the daemon re-created docs.kuzu after clean-docs"
    );
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --lib watch::tests -- --test-threads=1`
Expected: FAIL to compile (`run_attached_cycle`'s first argument is still `&Path`).

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test docs_ops clean_docs_is_not_undone -- --test-threads=1`
Expected: FAIL, "the watcher recreated the store clean-docs removed". The unlocked watcher's pending reindex opens the store after the delete. The race depends on timing before Step 4, which is why Step 4 closes it with the lock rather than a delay. If a run passes by luck, re-run it.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test docs_opt_in clean_docs_turns_docs_off -- --test-threads=1`
Expected: FAIL, "the daemon re-created docs.kuzu after clean-docs", on the same race.

- [ ] **Step 3: Gate the loop on the switch.** Replace `watch_docs_daemon_loop` and `run_attached_cycle` (L202-284):

```rust
/// Drive doc-watching for `root` as part of the merged code+doc daemon (see
/// `infigraph_core::daemon::lifecycle`). Attaches a `watch_docs` session
/// while `[docs] enabled` is on (`docs_switch::docs_enabled`), detaches when
/// it goes off, and re-attaches when it comes back. Documents are opt-in:
/// the switch decides, not whether `docs.kuzu` exists, and the first
/// catch-up reindex after attaching creates the store. Exits once
/// `shutdown` is set. Blocks until then.
///
/// Stopping and starting doc-watching is the daemon's `Control(Docs, ..)`
/// (#155): stop sets `shutdown` and joins this thread, start spawns a new
/// one. There is deliberately no stop file: a loop paused by a file is a
/// state the daemon cannot report.
pub fn watch_docs_daemon_loop(
    root: &Path,
    debounce_ms: u64,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let poll = attach_poll_interval(root);
    let enabled = || infigraph_core::docs_switch::docs_enabled(root);
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        if !enabled() {
            std::thread::sleep(poll);
            continue;
        }
        let root_owned = root.to_path_buf();
        eprintln!(
            "[doc-watch-daemon] attaching doc watcher for {}",
            root.display()
        );
        run_attached_cycle(enabled, &shutdown, poll, move |stop_rx| {
            watch_docs(&root_owned, debounce_ms, stop_rx, "doc-watch-daemon")
        });
    }
}

/// Drives one attach cycle: runs `watch_fn` (normally a `watch_docs` call) on
/// its own thread and polls, in the CALLING thread, for whichever trips
/// first: `watch_fn` finishing on its own (unrequested), `shutdown`, or
/// `still_wanted` turning false.
///
/// `handle.is_finished()` is checked before any of the other conditions on
/// every tick specifically so this function can never block forever: those
/// other conditions are things `watch_fn` reacts to via `stop_rx`, so once
/// `watch_fn` has already returned (e.g. `notify`'s sender was dropped, or
/// the watcher failed to start), none of them will necessarily ever become
/// true, and this function must notice that exit directly instead of
/// waiting on a stop signal nothing will act on.
fn run_attached_cycle<F>(
    still_wanted: impl Fn() -> bool,
    shutdown: &Arc<AtomicBool>,
    poll: Duration,
    watch_fn: F,
) where
    F: FnOnce(mpsc::Receiver<()>) -> Result<()> + Send + 'static,
{
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || watch_fn(stop_rx));

    let log_join_result = |res: std::thread::Result<Result<()>>| match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("[doc-watch-daemon] watch_docs error: {e}"),
        Err(_) => eprintln!("[doc-watch-daemon] watch_docs thread panicked"),
    };

    loop {
        if handle.is_finished() {
            log_join_result(handle.join());
            eprintln!("[doc-watch-daemon] watch_docs exited unexpectedly, retrying");
            return;
        }

        if shutdown.load(Ordering::Relaxed) {
            let _ = stop_tx.send(());
            log_join_result(handle.join());
            return;
        }

        if !still_wanted() {
            eprintln!("[doc-watch-daemon] detaching: document indexing is off");
            let _ = stop_tx.send(());
            log_join_result(handle.join());
            return;
        }

        std::thread::sleep(poll);
    }
}
```

In `attach_poll_interval`'s doc comment, replace "polls for `.infigraph/docs.kuzu`'s existence and the per-handler stop sentinel" with "re-reads `[docs] enabled`".

- [ ] **Step 4: Reindex under the docs lock, re-checking the switch.** In `watch_docs`, replace the whole `if pending && last_reindex.elapsed() >= debounce { … }` block (L76-118) with:

```rust
        if pending && last_reindex.elapsed() >= debounce {
            let indexed = match reindex_if_enabled(root, log_prefix) {
                // Another docs operation (index-docs, clean-docs, a read)
                // holds the lock: keep `pending` and try on the next tick
                // rather than block a stop behind it.
                Reindex::Busy => continue,
                Reindex::Done { indexed } => indexed,
            };
            if indexed {
                match crate::combined::schedule_group_doc_refresh(root) {
                    Ok(count) if count > 0 => {
                        eprintln!("[{log_prefix}] refreshing {count} combined document group(s)")
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("[{log_prefix}] combined refresh error: {e}"),
                }
            }
            pending = false;
            last_reindex = Instant::now();
        }
```

After `watch_docs`, add:

```rust
/// What one debounced reindex attempt came to.
enum Reindex {
    /// The docs lock is held elsewhere; try again next tick.
    Busy,
    /// Ran, or had nothing to do because docs are off. `indexed` is whether
    /// it wrote anything a combined group should pick up.
    Done { indexed: bool },
}

/// One reindex, under the docs lock and only while docs are on. The switch
/// is re-read *under* the lock: `clean-docs` turns it off before it takes
/// the lock to delete, so a reindex that gets the lock after it must not
/// recreate what it deleted.
fn reindex_if_enabled(root: &Path, log_prefix: &str) -> Reindex {
    let _op = match infigraph_core::docs_switch::try_lock_docs_op(root) {
        Ok(Some(guard)) => guard,
        Ok(None) => return Reindex::Busy,
        Err(e) => {
            eprintln!("[{log_prefix}] docs lock error: {e}");
            return Reindex::Done { indexed: false };
        }
    };
    if !infigraph_core::docs_switch::docs_enabled(root) {
        return Reindex::Done { indexed: false };
    }
    let _reindexing = ReindexGuard::enter();
    eprintln!("[{log_prefix}] document change detected, reindexing...");
    let mut idx = match DocIndex::open(root) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[{log_prefix}] open error: {e}");
            return Reindex::Done { indexed: false };
        }
    };
    if let Err(e) = idx.init() {
        eprintln!("[{log_prefix}] init error: {e}");
        return Reindex::Done { indexed: false };
    }
    match idx.index() {
        Ok(r) => {
            eprintln!(
                "[{log_prefix}] reindexed: {} files, {} chunks",
                r.indexed_files, r.total_chunks
            );
            Reindex::Done { indexed: true }
        }
        Err(e) => {
            eprintln!("[{log_prefix}] index error: {e}");
            Reindex::Done { indexed: false }
        }
    }
}
```

The group refresh now runs after the lock is released. Its build runs on a spawned thread and takes the shared lock on each source repo (Task 9), this one included.

- [ ] **Step 5: Run the watch tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --lib watch::tests -- --test-threads=1`
Expected: PASS, the four rewritten tests, the two new ones and every other `watch::tests` test included.

- [ ] **Step 6: Gate MCP's in-process auto-start on the switch.** In `auto_start_doc_watch_inner` (tools/docs.rs L77-79), replace

```rust
    if !root.join(".infigraph").join("docs.kuzu").exists() {
        return None;
    }
```

with

```rust
    if !infigraph_core::docs_switch::docs_enabled(&root) {
        return None;
    }
```

- [ ] **Step 7: Opt the MCP and CLI watcher tests in explicitly.** In `watcher_reindex.rs`'s `test_doc_watch_noop_before_doc_index_then_starts_after`, change the doc comment's first sentence to "auto_start_doc_watch only starts watching once the project has opted in to documents (`[docs] enabled`, per auto_start_doc_watch_inner)." Replace the two lines

```rust
    let idx = open_doc_index(&json!({"path": &path})).expect("open doc index");
    idx.index().expect("doc index");
```

with

```rust
    infigraph_core::docs_switch::set_docs_enabled(std::path::Path::new(&path), true)
        .expect("opt in");
```

Keep the comment above them, with "Index docs directly" changed to "Opt in directly". Change the assert message "should start watching once docs.kuzu exists" to "should start watching once docs are enabled". If `open_doc_index` is now unused in that file's import list, remove it.

In `watcher_daemon_mode.rs`, `auto_start_doc_watch_respects_daemon_mode_toggle` and `auto_start_doc_watch_respects_watch_docs_enabled_policy` would now pass vacuously: `auto_start_doc_watch` returns `None` before reaching the gate they test. In both, directly after the `infigraph_docs::DocIndex::open(&root).unwrap().init().unwrap();` statement, add:

```rust
    infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();
```

In `watch_daemon_docs.rs`'s `cmd_watch_daemon_also_indexes_docs_without_restart`, replace the comment and statement at L168-180 (`// Index docs for the same root WITHOUT stopping …` through `.unwrap();`) with:

```rust
    // Opt the project in WITHOUT stopping the watch process -- the daemon's
    // doc thread must notice the switch on its own, attach, and create the
    // store on its first catch-up.
    infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();
```

and change its `.expect(…)` message to `"daemon's doc thread never attached after docs were enabled"`. In `watch_docs_stop_and_start_over_control_are_visible_in_status`, replace the pre-create block at L548-555 with:

```rust
    // Opt in before the daemon starts, so its doc thread attaches on its
    // very first poll tick.
    infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();
```

and change the `.expect` at L621 to `"daemon's doc thread never attached on startup (docs were enabled)"`.

- [ ] **Step 8: Run the MCP, CLI and race tests**

Run: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-mcp --test watcher_reindex --test watcher_daemon_mode -- --test-threads=1`
Expected: PASS.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test watch_daemon_docs -- --test-threads=1`
Expected: PASS.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test docs_ops -- --test-threads=1`
Expected: PASS, all seven.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test docs_opt_in -- --test-threads=1`
Expected: PASS, all six.

- [ ] **Step 9: Commit**

```bash
git add crates/infigraph-docs/src/watch.rs crates/infigraph-mcp/src/tools/docs.rs crates/infigraph-mcp/tests/watcher_reindex.rs crates/infigraph-mcp/tests/watcher_daemon_mode.rs crates/infigraph-cli/tests/watch_daemon_docs.rs crates/infigraph-docs/tests/docs_ops.rs crates/infigraph-cli/tests/docs_opt_in.rs
git commit --no-verify -m "feat(docs): the doc watcher attaches on [docs] enabled and reindexes under the docs lock" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```


---

### Task 8: Readers never create a store; `search_docs` says why

**Files:**
- Modify: `crates/infigraph-docs/src/lib.rs` (`DocIndex` struct L30-40, `open` L52-65, add `open_existing`)
- Modify: `crates/infigraph-cli/src/search_commands.rs:175-192` (`cmd_search_docs`)
- Modify: `crates/infigraph-cli/src/pipeline_commands.rs` (four `DocIndex::open(root)?; idx.init()?;` pairs at L38-39, L56-57, L81-82, L112-113)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (`cmd_index_manifests` L160-162; `cmd_index_confluence` L1191-1192)
- Modify: `crates/infigraph-mcp/src/tools/docs.rs` (`open_doc_index` L16-24, `tool_search_docs` L231-232, `tool_index_confluence` L435-436, `tool_index_confluence_pages` L469-470)
- Modify: `crates/infigraph-mcp/src/tools/index.rs:284-294` (`tool_index_project`'s doc step)
- Test: `crates/infigraph-docs/tests/docs_ops.rs`, `crates/infigraph-mcp/tests/docs_opt_in.rs` (create), `crates/infigraph-cli/tests/docs_opt_in.rs`

**Interfaces:**
- Consumes: Task 1's `docs_indexed`, `lock_docs_read`, `DocsNotIndexed`. Task 5's `request_index_docs_if_enabled`. Task 6's `stats_report`.
- Produces: `pub fn DocIndex::open_existing(root: &Path) -> anyhow::Result<DocIndex>` (`Err(DocsNotIndexed)` unless opted in and indexed, remote mode excepted). `DocIndex` gains a private `read_lock: Option<LockFile>`, declared last so it drops after the store.

`DocStore::open` and `DocIndex::init` call-site audit (every production site; test fixtures open stores deliberately):

| Site | May it create the store? | Decision |
|---|---|---|
| `daemon_source.rs` startup probe | No | Task 3: `open_for_read` |
| `daemon_source.rs` per-request read | No | Task 3: `open_for_read` |
| `daemon_store.rs` `with_reader` (direct reads) | No | Task 3: `open_for_read` |
| `lib.rs` `DocIndex::init` local open | Yes, but reached only from writers | Writers: `ops::index_docs` (switch on first, exclusive lock), watcher `reindex_if_enabled` (exclusive lock, switch re-read). Readers go through `open_existing`, which checks first. |
| `lib.rs` `DocIndex::init` wipe-and-rebuild | Recreates a corrupt store that existed | Keep: it only follows a failed open of an existing store |
| `watch.rs` `watch_docs` reindex | Yes, when enabled | Task 7: under `try_lock_docs_op`, re-checks `docs_enabled` |
| `ops.rs` `index_docs` | Yes | Task 4: the executor |
| `info_commands.rs` `cmd_index_docs`/`cmd_reindex_docs` | Via the executor | Task 6 |
| `search_commands.rs` `cmd_search_docs` | No | This task: `open_existing` |
| `pipeline_commands.rs` ×4 | No | This task: `open_existing` |
| `info_commands.rs` `cmd_index_manifests` | No (links into an existing store) | This task: `open_existing` |
| `info_commands.rs` `cmd_index_confluence` | No (syncs into an existing store) | This task: `open_existing` |
| MCP `open_doc_index` (used by `search_docs`, `search` with `scope = "all"`, pipelines ×4, `index_manifests`, `tool_index_project`) | No | This task: `open_existing` |
| MCP `tool_index_confluence`, `tool_index_confluence_pages` | No | This task: `open_existing` |
| MCP `tool_index_project` doc step | Only for an opted-in project | This task: `request_index_docs_if_enabled` |
| MCP `tool_index_docs`/`tool_reindex_docs`/`tool_clean_docs` fallbacks | Via the executor / `clean_docs` | Task 6 |
| CLI `group_commands.rs` step 5, MCP `tool_group_build` step 5 | Only for an opted-in repo (local mode) | Task 9: `request_index_docs_if_enabled` |
| `combined.rs` source repo open | No | Task 9: skip when not enabled; `open_for_read` |
| `combined.rs` combined destination | Yes, in a fresh generation dir (a group artifact, not a project) | Keep |
| `combined.rs` `open_combined_docs` | No (already checks existence) | Keep |
| `combined.rs` `combined_doc_search` | No | Task 9: existence check before the open |

- [ ] **Step 1: Write the failing docs-crate tests.** Append to `crates/infigraph-docs/tests/docs_ops.rs`:

```rust
#[test]
fn open_existing_refuses_a_project_that_has_not_opted_in_and_creates_nothing() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    let err = infigraph_docs::DocIndex::open_existing(&root)
        .err()
        .expect("nothing to open");
    assert!(
        err.is::<infigraph_core::docs_switch::DocsNotIndexed>(),
        "{err}"
    );
    assert!(!root.join(".infigraph").exists());
}

#[test]
fn open_existing_refuses_an_opted_in_project_whose_store_is_missing() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();
    assert!(infigraph_docs::DocIndex::open_existing(&root).is_err());
    assert!(!docs_store_path(&root).exists(), "a reader created the store");
}

#[test]
fn open_existing_reads_an_opted_in_store() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    index_docs(&root, false).unwrap();
    let idx = infigraph_docs::DocIndex::open_existing(&root).unwrap();
    let hashes = idx.store().unwrap().get_doc_hashes().unwrap();
    assert!(hashes.contains_key("README.md"), "{hashes:?}");
}
```

`idx.store()` is a `&dyn DocBackend`, whose methods need no import.

- [ ] **Step 2: Write the failing MCP and CLI tests.** Create `crates/infigraph-mcp/tests/docs_opt_in.rs`:

```rust
//! MCP reads of a project that has not opted in to documents (docs opt-in).

use serde_json::json;

use infigraph_mcp::tools::docs::{open_doc_index, tool_search_docs};

/// `open_doc_index` is what `search` with `scope = "all"` calls on every
/// search: it must never create a document index.
#[test]
fn reading_documents_of_a_project_that_has_not_opted_in_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_string_lossy().to_string();

    let err = open_doc_index(&json!({ "path": &path }))
        .err()
        .expect("nothing to open");
    assert!(
        err.is::<infigraph_core::docs_switch::DocsNotIndexed>(),
        "{err}"
    );

    let out = tool_search_docs(&json!({ "path": &path, "query": "zebra" })).unwrap();
    assert_eq!(out, infigraph_core::docs_switch::DOCS_NOT_INDEXED);

    assert!(
        !dir.path().join(".infigraph").exists(),
        "a read must not create .infigraph/, let alone docs.kuzu"
    );
}
```

Append to `crates/infigraph-cli/tests/docs_opt_in.rs`:

```rust
/// Spec: `search_docs` on a project that is not enabled answers with the
/// message, and the search itself creates nothing.
#[test]
fn search_docs_on_a_fresh_project_says_how_to_opt_in_and_creates_nothing() {
    let (project, home) = project();
    let root = project.path();
    let _daemon = start_daemon(root, home.path());

    let out = run(root, home.path(), DAEMON, &["search-docs", "zebra"]);
    assert_ok(&out, "search-docs");
    assert!(
        stdout(&out).contains(infigraph_core::docs_switch::DOCS_NOT_INDEXED),
        "{}",
        stdout(&out)
    );
    assert!(!docs_store_path(root).exists());
    assert_eq!(docs_enabled_recorded(root), None);
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test docs_ops open_existing -- --test-threads=1`
Expected: FAIL to compile (`DocIndex::open_existing` not found).

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test docs_opt_in search_docs_on_a_fresh -- --test-threads=1`
Expected: FAIL. The output is the daemon's routed error wrapped by the CLI, or an empty result, not the message.

- [ ] **Step 4: Implement `open_existing`.** In `crates/infigraph-docs/src/lib.rs`, add a last field to `struct DocIndex`:

```rust
    /// The shared docs lock, held for this index's lifetime when it opened
    /// the store locally for reading (`open_existing`). Declared last so
    /// the store closes before the lock is released.
    read_lock: Option<infigraph_core::lockfile::LockFile>,
```

Add `read_lock: None,` to `DocIndex::open`'s struct literal. After `open`, add:

```rust
    /// Open and initialize the project's existing document index, for a
    /// reader or for a write into a store that must already exist
    /// (confluence, manifest links). It never creates one: a project that
    /// has not opted in, or whose store is missing, is `DocsNotIndexed`, and
    /// nothing is written. That includes `.infigraph/` itself, which `open`
    /// would create. Remote mode is unaffected. A local open holds the shared
    /// docs lock for the index's lifetime, so `clean-docs` cannot delete the
    /// store under it.
    pub fn open_existing(root: &Path) -> Result<Self> {
        use infigraph_core::docs_switch;

        let remote = infigraph_core::daemon::lifecycle::is_remote_backend();
        if !remote && !docs_switch::docs_indexed(root) {
            return Err(docs_switch::DocsNotIndexed.into());
        }
        // A routed or remote store is not opened in this process.
        let read_lock = if remote || infigraph_core::daemon_backend_selected() {
            None
        } else {
            let lock = docs_switch::lock_docs_read(root)?;
            if !docs_switch::docs_indexed(root) {
                return Err(docs_switch::DocsNotIndexed.into());
            }
            Some(lock)
        };
        let mut idx = Self::open(root)?;
        idx.read_lock = read_lock;
        idx.init()?;
        Ok(idx)
    }
```

- [ ] **Step 5: Move every reader onto it.** In each of these, replace the pair

```rust
    let mut idx = infigraph_docs::DocIndex::open(root)?;
    idx.init()?;
```

(with `&root` for the MCP confluence sites) by

```rust
    let idx = infigraph_docs::DocIndex::open_existing(root)?;
```

The sites are `pipeline_commands.rs` ×4, `cmd_index_confluence` (info_commands.rs L1191-1192), `tool_index_confluence` (docs.rs L435-436) and `tool_index_confluence_pages` (docs.rs L469-470).

In `cmd_index_manifests` (info_commands.rs L160-162), replace

```rust
    if let Ok(mut doc_idx) = infigraph_docs::DocIndex::open(root) {
        if doc_idx.init().is_ok() {
```

with a single `if let Ok(doc_idx) = infigraph_docs::DocIndex::open_existing(root) {`, and remove the matching closing brace of the dropped inner `if`.

Replace `open_doc_index`'s body (MCP docs.rs L17-23) with:

```rust
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path' argument")?;
    infigraph_docs::DocIndex::open_existing(std::path::Path::new(path))
```

Replace `cmd_search_docs`'s first three lines (search_commands.rs L176-178) with:

```rust
    let idx = match infigraph_docs::DocIndex::open_existing(root) {
        Ok(idx) => idx,
        Err(e) if e.is::<infigraph_core::docs_switch::DocsNotIndexed>() => {
            println!("{e}");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let store = idx.store().context("doc store not initialized")?;
```

In `tool_search_docs` (MCP docs.rs L231), replace `let idx = open_doc_index(args)?;` with:

```rust
    let idx = match open_doc_index(args) {
        Ok(idx) => idx,
        Err(e) if e.is::<infigraph_core::docs_switch::DocsNotIndexed>() => {
            return Ok(e.to_string())
        }
        Err(e) => return Err(e),
    };
```

In `tool_index_project` (MCP index.rs L284-294), replace the comment and the `match open_doc_index(args).and_then(|idx| idx.index()) { … }` with:

```rust
    // Documents are opt-in: refresh them only for a project that opted in
    // (`index_docs`), never opt one in as a side effect of indexing code.
    let docs_started = std::time::Instant::now();
    match infigraph_docs::ops::request_index_docs_if_enabled(std::path::Path::new(path)) {
        Ok(Some(stats)) => out.push_str(&format!(
            "{}\n",
            infigraph_docs::ops::stats_report("Document indexing", &stats, docs_started.elapsed())
        )),
        Ok(None) => {}
        Err(e) => out.push_str(&format!("warning: doc indexing failed: {e}\n")),
    }
```

If `open_doc_index` is no longer used in `index.rs`, remove it from that file's imports.

- [ ] **Step 6: Run the tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test docs_ops -- --test-threads=1`
Expected: PASS.

Run: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-mcp --test docs_opt_in --test watcher_reindex --test tool_dispatch -- --test-threads=1`
Expected: PASS.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli --test docs_opt_in -- --test-threads=1`
Expected: PASS, all seven.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-docs/src/lib.rs crates/infigraph-docs/tests/docs_ops.rs crates/infigraph-cli/src/search_commands.rs crates/infigraph-cli/src/pipeline_commands.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-mcp/src/tools/docs.rs crates/infigraph-mcp/src/tools/index.rs crates/infigraph-mcp/tests/docs_opt_in.rs crates/infigraph-cli/tests/docs_opt_in.rs
git commit --no-verify -m "fix(docs): readers never create a document index; search_docs says how to opt in" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```


---

### Task 9: Groups and `doctor`

**Files:**
- Modify: `crates/infigraph-docs/src/combined.rs` (source loop L76-90, `open_combined_docs` L395-404, `combined_doc_search` L406-414)
- Modify: `crates/infigraph-cli/src/group_commands.rs` (step 5 loop, after `let entry = …` at L503-506)
- Modify: `crates/infigraph-mcp/src/tools/groups.rs` (step 5 loop, after `let entry = …` at L522-525)
- Modify: `crates/infigraph-core/src/doctor.rs` (add `check_docs` after `check_sidecars` L1159-1170; `run_doctor` L1519-1540)
- Test: `crates/infigraph-docs/tests/group_build_docs.rs`, `crates/infigraph-docs/tests/combined_docs.rs`, `doctor.rs` tests

**Interfaces:**
- Consumes: Task 1's `docs_enabled`, `docs_store_path`, `DocsNotIndexed`. Task 3's `DocStore::open_for_read`. Task 5's `request_index_docs_if_enabled`.
- Produces: `pub fn doctor::check_docs(ctx: &DoctorContext) -> Vec<CheckResult>` and `fn doctor::check_one_project_docs(project_path: &Path) -> CheckResult`.

- [ ] **Step 1: Opt the existing combined-docs fixtures in, and write the failing tests.** In both `group_build_docs.rs` and `combined_docs.rs`, add as the first line of `index_doc`:

```rust
    infigraph_core::docs_switch::set_docs_enabled(root, true).unwrap();
```

This is what an index in production implies. Every existing assertion stays as it is.

Append to `group_build_docs.rs`:

```rust
/// Docs are opt-in per repo: a repo that turned them off contributes
/// nothing to a combined store, even with its old store still on disk.
#[test]
fn test_step5_combined_docs_skips_a_repo_with_documents_turned_off() {
    let _guard = GROUP_BUILD_DOCS_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let old_home = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    index_doc(
        repo_a.path(),
        "README.md",
        "# Order Service\n\nHandles order payments.",
    );
    infigraph_core::docs_switch::set_docs_enabled(repo_a.path(), false).unwrap();

    let registry = two_repo_registry(repo_a.path(), repo_b.path());
    let stats = build_combined_docs(&registry, "docs-steps-group").unwrap();
    assert_eq!(stats.documents, 0, "a repo with docs off contributed documents");

    if let Some(h) = old_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
}
```

In `combined_docs.rs`'s `combined_docs_nonexistent_group`, after the `combined_doc_query` assertion, add:

```rust
    // Searching a group that was never built is an error, and must not
    // create an empty combined store as a side effect.
    assert!(combined_doc_search("nonexistent", "anything", 5, 0.5).is_err());
    assert!(!has_combined_docs("nonexistent"));
```

Add the doctor tests at the end of `doctor.rs`:

```rust
#[cfg(test)]
mod docs_check_tests {
    use super::*;
    use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};

    fn check(enabled: Option<bool>, store: bool) -> CheckResult {
        let dir = tempfile::tempdir().unwrap();
        if let Some(on) = enabled {
            crate::docs_switch::set_docs_enabled(dir.path(), on).unwrap();
        }
        if store {
            let path = crate::docs_switch::docs_store_path(dir.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }
        check_one_project_docs(dir.path())
    }

    /// `HOME` is restored before the lock is released: a tuple drops its
    /// fields in order.
    fn isolated() -> (PinnedHome, std::sync::MutexGuard<'static, ()>) {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_DOCS_ENABLED");
        (PinnedHome::empty(), guard)
    }

    #[test]
    fn docs_on_with_an_index_passes() {
        let _env = isolated();
        let r = check(Some(true), true);
        assert_eq!(r.status, CheckStatus::Pass, "{}", r.message);
        assert!(r.message.contains("enabled"), "{}", r.message);
    }

    #[test]
    fn docs_off_with_no_index_passes() {
        let _env = isolated();
        let r = check(None, false);
        assert_eq!(r.status, CheckStatus::Pass, "{}", r.message);
        assert!(r.message.contains("disabled"), "{}", r.message);
    }

    #[test]
    fn docs_on_without_an_index_warns_to_index() {
        let _env = isolated();
        let r = check(Some(true), false);
        assert_eq!(r.status, CheckStatus::Warn, "{}", r.message);
        assert!(r.remediation.unwrap().contains("index-docs"));
    }

    #[test]
    fn an_index_left_while_docs_are_off_warns_to_clean() {
        let _env = isolated();
        let r = check(Some(false), true);
        assert_eq!(r.status, CheckStatus::Warn, "{}", r.message);
        assert!(r.remediation.unwrap().contains("clean-docs"));
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test group_build_docs --test combined_docs -- --test-threads=1`
Expected: FAIL. `test_step5_combined_docs_skips_a_repo_with_documents_turned_off` finds 1 document. `combined_docs_nonexistent_group` fails because `has_combined_docs` is true after the search.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib doctor::docs_check_tests -- --test-threads=1`
Expected: FAIL to compile (`check_one_project_docs` not found).

- [ ] **Step 3: Groups.** In `build_combined_docs`, replace L81-90 (`let source_path = …` through `let store = DocStore::open(&source_path)?;`) with:

```rust
        if !infigraph_core::docs_switch::docs_enabled(&entry.path) {
            eprintln!(
                "  [combined-docs] skip {} — document indexing is off",
                repo_name
            );
            continue;
        }
        let store = match DocStore::open_for_read(&entry.path) {
            Ok(store) => store,
            Err(e) if e.is::<infigraph_core::docs_switch::DocsNotIndexed>() => {
                eprintln!(
                    "  [combined-docs] skip {} — documents not indexed",
                    repo_name
                );
                continue;
            }
            Err(e) => return Err(e),
        };
```

Above `open_combined_docs`, add:

```rust
fn combined_store_not_found(group_name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Combined document store not found for group '{}'. Run group_build first.",
        group_name
    )
}
```

Replace `open_combined_docs`'s `anyhow::bail!(…)` with `return Err(combined_store_not_found(group_name));`. In `combined_doc_search`, after `let store_path = artifact_dir.join("docs.kuzu");`, add:

```rust
    // Opening a missing store creates it; a search must not.
    if !store_path.exists() {
        return Err(combined_store_not_found(group_name));
    }
```

In `group_commands.rs`'s step 5 loop, directly after the `let entry = registry.repos.get(repo_name).context(…)?;` statement, add:

```rust
                // Documents are opt-in per repo (local mode): refresh the
                // ones that opted in, through the daemon when it owns the
                // store, and leave the rest alone.
                if !is_remote {
                    if let Some(stats) =
                        infigraph_docs::ops::request_index_docs_if_enabled(&entry.path)?
                    {
                        bfs_discovered += stats.bfs_discovered;
                    }
                    continue;
                }
```

Add the same block (with the same comment) in `tool_group_build`'s step 5 loop (MCP groups.rs) directly after its `let entry = …?;`.

- [ ] **Step 4: `doctor`.** After `check_sidecars`, add:

```rust
const DOCS_CATEGORY: &str = "documents";

/// Document indexing is opt-in per project: say whether it is on, and
/// whether `docs.kuzu` agrees with the switch.
fn check_one_project_docs(project_path: &Path) -> CheckResult {
    let enabled = crate::docs_switch::docs_enabled(project_path);
    let present = crate::docs_switch::docs_store_path(project_path).exists();
    let label = format!("{}: documents", project_path.display());
    let cd = project_path.display();
    match (enabled, present) {
        (true, true) => CheckResult::pass(DOCS_CATEGORY, label, "enabled; docs.kuzu present"),
        (false, false) => CheckResult::pass(
            DOCS_CATEGORY,
            label,
            "disabled (opt in with `infigraph index-docs`)",
        ),
        (true, false) => CheckResult::warn(
            DOCS_CATEGORY,
            label,
            "enabled, but docs.kuzu is missing",
            format!("run `cd {cd} && infigraph index-docs`"),
        ),
        (false, true) => CheckResult::warn(
            DOCS_CATEGORY,
            label,
            "disabled, but docs.kuzu is still on disk",
            format!("run `cd {cd} && infigraph clean-docs` to reclaim the space"),
        ),
    }
}

pub fn check_docs(ctx: &DoctorContext) -> Vec<CheckResult> {
    projects_in_scope(ctx)
        .iter()
        .map(|p| check_one_project_docs(p))
        .collect()
}
```

In `run_doctor`, add `checks.extend(check_docs(&ctx));` after `checks.extend(check_sidecars(&ctx));`.

- [ ] **Step 5: Run the tests**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs --test group_build_docs --test combined_docs -- --test-threads=1`
Expected: PASS.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib doctor -- --test-threads=1`
Expected: PASS.

Run: `cargo build -p infigraph-cli -p infigraph-mcp`
Expected: builds with no errors or warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-docs/src/combined.rs crates/infigraph-cli/src/group_commands.rs crates/infigraph-mcp/src/tools/groups.rs crates/infigraph-core/src/doctor.rs crates/infigraph-docs/tests/group_build_docs.rs crates/infigraph-docs/tests/combined_docs.rs
git commit --no-verify -m "feat(docs): groups include only opted-in repos; doctor reports the docs switch" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```


---

### Task 10: Documentation, the invariant, full verification

**Files:**
- Modify: `docs/DOCUMENT-INDEXING.md` (new section after "Entry Points", L59-100; table of contents L7-24)
- Modify: `CLAUDE.md` (the "exactly one `Database` per graph file" bullet, and a new bullet after it)
- Modify: `AGENTS.md` (the same bullet, and the same new bullet)

**Interfaces:**
- Consumes: everything above.
- Produces: documentation only.

- [ ] **Step 1: `docs/DOCUMENT-INDEXING.md`.** Add `3. [Opting In](#opting-in)` to the table of contents after "Entry Points", and renumber the entries after it. Insert before `## File Discovery`:

```markdown
## Opting In

Document indexing is **off** for a new project. A project opts in by
running `infigraph index-docs` (or MCP `index_docs`) once, or by setting
`[docs] enabled = true` in `.infigraph/config.toml` (env:
`INFIGRAPH_DOCS_ENABLED`). `infigraph clean-docs` opts it back out and
deletes the index. A project that already had a `docs.kuzu` when this
shipped was recorded as opted in at its next daemon start, unless its
`config.toml` already said otherwise.

- **Where it runs.** Under the default daemon backend, `index-docs` asks
  the daemon to index (`WriteRequest::IndexDocs`). The daemon runs the one
  executor, `infigraph_docs::ops::index_docs`, beside the store it owns.
  With `INFIGRAPH_BACKEND=kuzu` the CLI runs the same executor itself.
- **The doc watcher** attaches while `[docs] enabled` is on and detaches
  when it goes off. Its first catch-up reindex creates `docs.kuzu`.
- **Reads create nothing.** A read of a project without a store answers
  "documents are not indexed for this project; run `infigraph
  index-docs`". It never opens (and so never creates) `docs.kuzu`, or even
  `.infigraph/`.
- **One operation at a time.** `.infigraph/docs-op.lock` is taken
  exclusively by anything that may create or delete the store (the
  watcher's reindex, `index-docs`, `clean-docs`) and shared by reads, across
  processes.
- **Groups** include a repository's documents only if that repository has
  opted in. `infigraph doctor` reports each project's switch and whether
  `docs.kuzu` agrees with it.
```

In the "MCP Tools" and "CLI" subsections, add one sentence each: "`index_docs` opts the project in; `clean_docs` opts it out."

- [ ] **Step 2: The invariant.** In `CLAUDE.md` and `AGENTS.md`, in the bullet that starts "**There is exactly one `Database` per graph file in a process.**", replace its last two sentences ("The documents side is the deliberate exception … That is safe only because the docs writer does not stay open.") with:

```markdown
The documents side is the deliberate exception — it opens `docs.kuzu` per request, and only when the file exists (`DocStore::open_for_read`), because `DocStore::open` holds the process-wide `DB_LOCK` for the store's lifetime and a long-lived one would block the doc watcher's next `DocIndex::init()` forever. That is safe only because the docs writer does not stay open.
```

Directly after that bullet, add:

```markdown
- **Document indexing is opt-in per project.** `infigraph_core::docs_switch::docs_enabled(root)` (`[docs] enabled`, default off) is the only question; nothing decides from whether `docs.kuzu` exists. Three writers record it, all through `set_docs_enabled`: the executor `infigraph_docs::ops::index_docs` (on), `ops::clean_docs` (off), and the daemon-start migration (on, only when the key is absent). `DocStore::open` creates a missing store, so only writers may reach it: the executor and the doc watcher's reindex, both under `.infigraph/docs-op.lock` held exclusively, with the watcher re-reading the switch under the lock. Readers go through `DocIndex::open_existing` / `DocStore::open_for_read`, which check for the store before taking the shared lock (a lock file creates its directory, and a read must not bring back a removed `.infigraph/`). A new document reader uses one of those two, never `DocIndex::open` + `init`.
```

- [ ] **Step 3: Format and lint the workspace**

Run: `cargo fmt --all`
Expected: no output.

Run: `cargo clippy --all-targets -- -D warnings`
Expected: `Finished` with no warnings.

- [ ] **Step 4: Run every touched crate's full suite, one crate at a time**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core -- --test-threads=1`
Expected: PASS.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-docs -- --test-threads=1`
Expected: PASS.

Run: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli -- --test-threads=1`
Expected: PASS.

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_DOCS_ENABLED INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-mcp -- --test-threads=1`
Expected: PASS.

A failure under this load is first re-run alone at `--test-threads=1` (CLAUDE.md's contention note) before it is treated as a regression.

- [ ] **Step 5: Commit with the full hook**

```bash
git add docs/DOCUMENT-INDEXING.md CLAUDE.md AGENTS.md
git commit -m "docs: document indexing is opt-in per project" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01P2gYhrT311Ww8ejixctsyV"
```

Expected: the pre-commit hook's fmt, clippy and perf gates pass. If the hook's perf gates are skipped for a docs-only commit, run `cargo test --all` once here instead, as CLAUDE.md requires before a change is done.
