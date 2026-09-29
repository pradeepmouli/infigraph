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

/// How long a read waits behind a docs operation before it gives up. Far
/// shorter than [`DOCS_OP_WAIT`]: a writer may legitimately take minutes, but
/// a search should tell its caller to retry, not hang.
pub const DOCS_READ_WAIT: Duration = Duration::from_secs(10);

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

/// Take the docs lock shared, for a read, waiting up to [`DOCS_READ_WAIT`].
/// The lock file's directory is created if missing, so a caller checks that
/// the store exists first: a read must never bring back an `.infigraph/`
/// someone removed.
pub fn lock_docs_read(root: &Path) -> Result<crate::lockfile::LockFile> {
    lock_docs_read_within(root, DOCS_READ_WAIT)
}

/// [`lock_docs_read`] with an explicit wait. A read stuck behind a long
/// `index-docs` gives up with a message a person can act on, rather than
/// stalling a whole cross-scope search for the writers' ten minutes.
pub fn lock_docs_read_within(root: &Path, wait: Duration) -> Result<crate::lockfile::LockFile> {
    crate::lockfile::acquire_shared(&docs_op_lock_path(root), wait).map_err(|e| {
        if e.is::<crate::lockfile::Busy>() {
            e.context("documents are being indexed or cleaned right now; try again shortly")
        } else {
            e
        }
    })
}

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
        assert!(
            !docs_enabled(tmp.path()),
            "the project wins the key it states"
        );
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
        assert!(
            !docs_indexed(tmp.path()),
            "the switch alone has nothing to read"
        );
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

    /// A reader behind a long `index-docs` gives up soon and says why; it
    /// does not sit for the writers' ten minutes.
    #[test]
    fn a_read_behind_a_docs_operation_gives_up_and_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        let _held = lock_docs_op(tmp.path(), Duration::from_secs(1)).unwrap();
        let err = lock_docs_read_within(tmp.path(), Duration::from_millis(50))
            .expect_err("the shared lock was granted under an exclusive holder");
        let text = format!("{err:#}");
        assert!(text.contains("try again"), "{text}");
        assert!(DOCS_READ_WAIT < DOCS_OP_WAIT);
    }

    #[test]
    fn not_indexed_reads_as_its_message() {
        assert_eq!(DocsNotIndexed.to_string(), DOCS_NOT_INDEXED);
        let err = anyhow::Error::new(DocsNotIndexed);
        assert!(err.is::<DocsNotIndexed>());
    }

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
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "[docs\nenabled = "
        );
    }
}
