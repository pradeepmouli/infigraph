//! Where `.infigraph/config.toml` lives, and in what order its layers
//! apply.
//!
//! Its readers parse the same file differently and for good reason --
//! `watch::config` uses `toml_edit` so a surgical per-key write cannot drop
//! sections it has no schema for, while `infigraph-mcp`'s `session_context`
//! deserializes into typed structs. What they must agree on is *which files
//! count and which wins*, and that is what lives here, along with the one
//! shared loader, [`layers`], that `settings!` groups read through.
//!
//! They did not agree. `watch::config` read only the project file and never
//! `$HOME`; `session_context` walked up from the process cwd and then fell
//! back to `$HOME`. Worse than inconsistent, the fallback made the mere
//! *existence* of a project file -- not its contents -- decide whether any
//! user-level setting applied at all, and `infigraph watch disable` creates
//! a project file holding nothing but `[watch]`. Running it silently
//! stopped a user's global compression settings from applying to that
//! project.
//!
//! # The rule
//!
//! Two layers, **merged per key**, nearer wins:
//!
//! 1. the project's `<root>/.infigraph/config.toml`
//! 2. the user's `~/.infigraph/config.toml`
//!
//! Per key is the load-bearing part. A layer that says nothing about a key
//! must fall through to the one below rather than blanking it, which is
//! what separates a layer from a fallback.
//!
//! This also answers `watch::config`'s original objection to consulting
//! `$HOME` -- that a machine-wide value would apply "with no per-project
//! override". Under a layered read the override exists by construction: the
//! project layer wins every key it states.
//!
//! # Settings groups (#160)
//!
//! [`layers`] is the third reader, the one every `settings!` group's
//! `resolve` goes through. It keeps each file as a `toml_edit` document and
//! lets the macro pick out its own `[category]` section, nearest layer first.
//!
//! Which layers apply is the caller's [`ConfigScope`], and a project layer
//! needs a root the caller actually has. Never the process cwd: a daemon's
//! or MCP server's cwd routinely differs from the project it serves, so an
//! ambient root silently applies some other project's settings. A setting
//! with no root in hand -- or one that describes this process or machine
//! rather than a project -- reads the user layer alone.
//!
//! `session_context`'s `[compression]` reader still walks up from
//! `current_dir()`. Compression is per MCP session rather than per project,
//! and that server is launched from the project it serves.

use std::collections::HashMap;

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// The project layer: `<root>/.infigraph/config.toml`. Returned whether or
/// not it exists -- callers treat an unreadable file as "states nothing".
pub fn project_config_path(root: &Path) -> PathBuf {
    root.join(".infigraph").join("config.toml")
}

/// The user layer: `~/.infigraph/config.toml`, or `None` when there is no
/// home directory or no such file.
pub fn user_config_path() -> Option<PathBuf> {
    user_config_location().filter(|candidate| candidate.exists())
}

/// Where the user layer would be, whether or not it exists. `USERPROFILE`
/// is the Windows spelling of `HOME`.
fn user_config_location() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".infigraph").join("config.toml"))
}

/// Which `config.toml` layers a settings group consults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope<'a> {
    /// The project at this root, over the user layer.
    Project(&'a Path),
    /// The user layer alone: a setting of this process or machine, or one
    /// read where no project root is in hand.
    User,
}

impl<'a> ConfigScope<'a> {
    /// The scope of a store living in `infigraph_dir`: that project when
    /// the directory really is a `.infigraph`, the user layer otherwise.
    ///
    /// The name check matters. A store opened on some other directory (an
    /// in-memory store has none; tests use bare tempdirs) has no project
    /// root, and taking its parent anyway would read whatever
    /// `.infigraph/config.toml` happens to sit beside it.
    pub fn of_infigraph_dir(infigraph_dir: Option<&'a Path>) -> Self {
        infigraph_dir
            .filter(|dir| dir.file_name() == Some(std::ffi::OsStr::new(".infigraph")))
            .and_then(Path::parent)
            .map_or(ConfigScope::User, ConfigScope::Project)
    }
}

/// The config documents `scope` consults, nearest first. A missing or
/// unparseable file is `None`: it states nothing.
pub fn layers(scope: ConfigScope<'_>) -> [Option<Arc<toml_edit::DocumentMut>>; 2] {
    let project = match scope {
        ConfigScope::Project(root) => load(&project_config_path(root)),
        ConfigScope::User => None,
    };
    [project, user_config_location().and_then(|path| load(&path))]
}

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
    // Read-modify-write under a lock: two writers in one daemon (the docs
    // executor and a watch-policy control) must not each start from the same
    // text and drop the other's key.
    let _rmw = crate::lockfile::acquire(
        &config_path.with_file_name("config.lock"),
        "config-write",
        std::time::Duration::from_secs(10),
    )
    .with_context(|| format!("locking {} for writing", config_path.display()))?;
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
    if doc.get(section).is_none() {
        // A real `[section]` table, not the inline `section = { .. }` that
        // indexing a missing key would produce: a later hand-written
        // `[section]` would then be a duplicate key and the file unparseable.
        doc[section] = toml_edit::table();
    }
    doc[section][key] = value;
    crate::daemon_protocol::write_atomic(&config_path, &doc.to_string())
}

/// A file's identity for cache purposes. Length alongside mtime catches a
/// rewrite that lands within a coarse filesystem's timestamp resolution.
type Fingerprint = Option<(SystemTime, u64)>;

struct Cached {
    fingerprint: Fingerprint,
    doc: Option<Arc<toml_edit::DocumentMut>>,
}

/// Parsed config files, keyed by path.
///
/// Settings resolve on hot paths -- `lockfile::slow_wait_threshold` runs on
/// every acquire while the write lock is held -- so a file is parsed once
/// per version, not once per read. Each read still stats the file, which is
/// the whole invalidation story: an edited file is picked up on the next
/// read, with no TTL to reason about.
static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);

fn load(path: &Path) -> Option<Arc<toml_edit::DocumentMut>> {
    let fingerprint: Fingerprint = std::fs::metadata(path)
        .ok()
        .and_then(|meta| Some((meta.modified().ok()?, meta.len())));

    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(cached) = cache.get(path) {
        if cached.fingerprint == fingerprint {
            return cached.doc.clone();
        }
    }

    // Parsed under the lock: rare (once per file version), and it keeps two
    // racing readers from each warning about the same broken file.
    let doc = fingerprint.and_then(|_| parse(path)).map(Arc::new);
    cache.insert(
        path.to_path_buf(),
        Cached {
            fingerprint,
            doc: doc.clone(),
        },
    );
    doc
}

fn parse(path: &Path) -> Option<toml_edit::DocumentMut> {
    let text = std::fs::read_to_string(path).ok()?;
    match text.parse() {
        Ok(doc) => Some(doc),
        Err(e) => {
            // Once per file version (the cache remembers the failure), and
            // never silently: a typo would otherwise quietly revert every
            // setting in the file to its default.
            eprintln!("warning: ignoring {}: not valid TOML: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(root: &Path, text: &str) -> PathBuf {
        let path = project_config_path(root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn port(doc: &toml_edit::DocumentMut) -> Option<i64> {
        doc.get("svc")?.get("port")?.as_integer()
    }

    /// A section that does not exist yet is written as `[section]`, not an
    /// inline `section = { key = value }` a later hand-written `[section]`
    /// would collide with (a duplicate key, so the whole file stops parsing).
    #[test]
    fn an_absent_section_is_written_as_a_table() {
        let tmp = tempfile::tempdir().unwrap();
        set_project_setting(tmp.path(), "docs", "enabled", toml_edit::value(true)).unwrap();
        let text = std::fs::read_to_string(project_config_path(tmp.path())).unwrap();
        assert!(text.contains("[docs]"), "{text}");
        assert!(!text.contains("docs = {"), "{text}");
        let mut extended = text;
        extended.push_str("\n[docs.extra]\nx = 1\n");
        assert!(extended.parse::<toml_edit::DocumentMut>().is_ok());
    }

    /// Two writers in one process (the docs executor and a watch-policy
    /// control, in the daemon) must both land: no lost update, and no shared
    /// temp file for one to clobber under the other.
    #[test]
    fn concurrent_writers_keep_every_update() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let threads: Vec<_> = (0..6)
            .map(|t| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for i in 0..8 {
                        set_project_setting(
                            &root,
                            "svc",
                            &format!("k{t}_{i}"),
                            toml_edit::value(i as i64),
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let doc = load(&project_config_path(&root)).unwrap();
        let svc = doc.get("svc").unwrap();
        for t in 0..6 {
            for i in 0..8 {
                assert!(svc.get(format!("k{t}_{i}")).is_some(), "lost k{t}_{i}");
            }
        }
    }

    #[test]
    fn an_edited_file_is_reread() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(tmp.path(), "[svc]\nport = 1\n");
        assert_eq!(load(&path).as_deref().and_then(port), Some(1));

        // A different length, so the fingerprint changes even on a
        // filesystem whose mtime cannot tell the two writes apart.
        std::fs::write(&path, "[svc]\nport = 1234\n").unwrap();
        assert_eq!(load(&path).as_deref().and_then(port), Some(1234));
    }

    #[test]
    fn a_file_created_after_a_miss_is_seen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = project_config_path(tmp.path());
        assert!(load(&path).is_none());

        write_config(tmp.path(), "[svc]\nport = 7\n");
        assert_eq!(load(&path).as_deref().and_then(port), Some(7));
    }

    #[test]
    fn an_unparseable_file_states_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config(tmp.path(), "[svc\nport = ");
        assert!(load(&path).is_none());
    }

    #[test]
    fn a_project_scope_puts_the_project_file_first() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "[svc]\nport = 5\n");
        let [project, _user] = layers(ConfigScope::Project(tmp.path()));
        assert_eq!(project.as_deref().and_then(port), Some(5));

        let [project, _user] = layers(ConfigScope::User);
        assert!(project.is_none(), "a user scope has no project layer");
    }

    #[test]
    fn only_a_directory_named_infigraph_implies_a_project() {
        let tmp = tempfile::tempdir().unwrap();
        let dot = tmp.path().join(".infigraph");
        assert_eq!(
            ConfigScope::of_infigraph_dir(Some(&dot)),
            ConfigScope::Project(tmp.path())
        );
        assert_eq!(
            ConfigScope::of_infigraph_dir(Some(tmp.path())),
            ConfigScope::User
        );
        assert_eq!(ConfigScope::of_infigraph_dir(None), ConfigScope::User);
    }

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
        let err =
            set_project_setting(tmp.path(), "docs", "enabled", toml_edit::value(true)).unwrap_err();
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
}

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
