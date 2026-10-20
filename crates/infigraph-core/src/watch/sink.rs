//! What a watcher's events are offered to: one subscription, triaged once,
//! then handed to every attached sink whose domain keeps the path.
//!
//! Two watchers used to subscribe to the same root (the code producer and the
//! document watcher), each with its own ignore matcher, `skips` check,
//! debounce and liveness logic. The rules for "can this event matter" now live
//! here, in [`Triage`], and a consumer is a [`WatchSink`] over a
//! [`PathIndexer`] -- the one operation every indexer offers: index these
//! paths, delete these paths, redo everything.
//!
//! The producer owns the only `RecommendedWatcher`; it respells each event path
//! ([`Triage::respelled`]) and asks [`Triage::route`] which domains keep it.

use crate::ignore_rules::{Domain, IgnoreMatcher};
use anyhow::Result;
use notify::EventKind;
use std::path::{Path, PathBuf};

/// What happened to one path, as a sink sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathChange {
    Created,
    Modified,
    Removed,
}

/// The changes one event (or one debounce window) brings to a sink.
/// Paths are relative to the watched root and spelled with `/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchBatch {
    pub changes: Vec<(PathBuf, PathChange)>,
    /// Events were lost (a sink's queue overflowed, the backend dropped some,
    /// a subscription was rebuilt): the sink answers with its own whole-project
    /// pass, [`PathIndexer::index_all`], since the list above is incomplete.
    pub rescan: bool,
}

impl WatchBatch {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && !self.rescan
    }

    /// A batch that says only "events were lost".
    pub fn rescan() -> Self {
        Self {
            changes: Vec::new(),
            rescan: true,
        }
    }
}

/// What one [`PathIndexer`] call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathOutcome {
    /// How many files (documents) the call wrote or removed.
    pub files: usize,
}

impl PathOutcome {
    /// Something in the store changed.
    pub fn wrote(&self) -> bool {
        self.files > 0
    }
}

/// The operation both indexers share. A sink drives one of these; it knows
/// nothing about graphs or documents.
///
/// Paths are root-relative. A path an indexer was asked to index that is no
/// longer on disk is a deletion, so a sink need not tell a rename's two halves
/// apart.
pub trait PathIndexer {
    /// Index these paths (and only these).
    fn index_paths(&self, paths: &[PathBuf]) -> Result<PathOutcome>;

    /// Remove the stored rows of these paths. A path that named a directory
    /// removes everything under it, and nothing beside it: `docs/a` must not
    /// remove `docs/ab.md`.
    fn delete_paths(&self, paths: &[PathBuf]) -> Result<PathOutcome>;

    /// The whole-project pass: finds what no event reported. Run on a
    /// [`WatchBatch::rescan`], when the sink attaches, and on a slow timer.
    fn index_all(&self) -> Result<PathOutcome>;
}

/// A sink's counters, for the status line and the degraded modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SinkHealth {
    /// Batches taken from the producer.
    pub accepted: u64,
    /// Batches dropped because the sink could not keep up; the next batch it
    /// takes carries [`WatchBatch::rescan`].
    pub dropped: u64,
}

/// A consumer of triaged events. `accept` never blocks the producer.
pub trait WatchSink: Send {
    fn name(&self) -> &'static str;

    /// Which ignore rules decide whether a path reaches this sink.
    fn domain(&self) -> Domain;

    fn accept(&mut self, batch: WatchBatch);

    fn health(&self) -> SinkHealth;

    /// Called on the producer's timer, for a sink that debounces.
    fn tick(&mut self) {}
}

/// The change an event kind reports for a path, if it reports one.
pub fn change_of(kind: &EventKind) -> Option<PathChange> {
    match kind {
        EventKind::Create(_) => Some(PathChange::Created),
        EventKind::Modify(_) => Some(PathChange::Modified),
        EventKind::Remove(_) => Some(PathChange::Removed),
        _ => None,
    }
}

/// Whether `kind` says a path came into being (created, or renamed in), as
/// opposed to a path that was already there changing.
///
/// A directory warrants attention only when it arrives: FSEvents reports a
/// directory, and the project root, as modified whenever anything inside is
/// written, and treating that as news made 8,000 reindexes of nothing in a
/// day on one worktree.
pub fn directory_arrived(kind: &EventKind) -> bool {
    use notify::event::ModifyKind;
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// Decides, once per event path, which domains keep it. Holds one
/// [`IgnoreMatcher`] per attached domain; the domain-to-matcher mapping stays
/// in `ignore_rules`.
pub struct Triage {
    root: PathBuf,
    matchers: Vec<(Domain, IgnoreMatcher)>,
}

impl Triage {
    pub fn new(root: &Path, domains: &[Domain]) -> Self {
        let mut triage = Self {
            root: root.to_path_buf(),
            matchers: Vec::new(),
        };
        for domain in domains {
            triage.attach(*domain);
        }
        triage
    }

    /// Starts judging `domain`'s paths. A domain already attached is kept.
    pub fn attach(&mut self, domain: Domain) {
        if !self.matchers.iter().any(|(d, _)| *d == domain) {
            self.matchers
                .push((domain, IgnoreMatcher::build(&self.root, domain)));
        }
    }

    /// Stops judging `domain`'s paths.
    pub fn detach(&mut self, domain: Domain) {
        self.matchers.retain(|(d, _)| *d != domain);
    }

    pub fn domains(&self) -> Vec<Domain> {
        self.matchers.iter().map(|(d, _)| *d).collect()
    }

    /// Rebuilds every matcher, so an edit to an ignore file reaches a
    /// long-lived watcher (every `ignore_rules::MATCHER_REBUILD_SECS`).
    pub fn rebuild(&mut self) {
        for (domain, matcher) in &mut self.matchers {
            *matcher = IgnoreMatcher::build(&self.root, *domain);
        }
    }

    /// `path` spelled under the root as given: a backend that reports resolved
    /// paths (FSEvents) would otherwise fail every `strip_prefix` on it.
    pub fn respelled<'a>(&self, path: &'a Path) -> std::borrow::Cow<'a, Path> {
        match self.matchers.first() {
            Some((_, matcher)) => matcher.respelled(path),
            None => std::borrow::Cow::Borrowed(path),
        }
    }

    /// The batch each domain gets for one path of one event. A domain whose
    /// rules skip the path (ignored, or a hidden component the walk would not
    /// visit) gets nothing, and neither does a directory that merely changed.
    pub fn route(&self, kind: &EventKind, path: &Path) -> Vec<(Domain, WatchBatch)> {
        let Some(change) = change_of(kind) else {
            return Vec::new();
        };
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return Vec::new();
        };
        let rel = PathBuf::from(rel.to_string_lossy().replace('\\', "/"));
        let is_dir = path.is_dir();
        let mut routed = Vec::new();
        for (domain, matcher) in &self.matchers {
            if matcher.skips(path, is_dir) {
                continue;
            }
            if is_dir {
                // Filled in with the directory's files by the next change.
                continue;
            }
            routed.push((
                *domain,
                WatchBatch {
                    changes: vec![(rel.clone(), change)],
                    rescan: false,
                },
            ));
        }
        routed
    }
}

#[cfg(test)]
pub(crate) mod fakes {
    //! One fake of each seam, used by every test of the machinery above them.
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A sink that records what it was given.
    pub struct FakeSink {
        pub domain: Domain,
        pub seen: Arc<Mutex<Vec<WatchBatch>>>,
    }

    impl FakeSink {
        pub fn new(domain: Domain) -> (Self, Arc<Mutex<Vec<WatchBatch>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    domain,
                    seen: Arc::clone(&seen),
                },
                seen,
            )
        }
    }

    impl WatchSink for FakeSink {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn domain(&self) -> Domain {
            self.domain
        }
        fn accept(&mut self, batch: WatchBatch) {
            self.seen.lock().unwrap().push(batch);
        }
        fn health(&self) -> SinkHealth {
            SinkHealth::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::FakeSink;
    use super::*;
    use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};
    use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind};
    use std::fs;

    fn create() -> EventKind {
        EventKind::Create(CreateKind::File)
    }

    fn modify() -> EventKind {
        EventKind::Modify(ModifyKind::Data(DataChange::Content))
    }

    fn write(root: &Path, rel: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "x").unwrap();
        path
    }

    /// A project whose code domain leaves `docs/` out and whose docs domain
    /// leaves `src/` out, so a path's domain is visible in where it goes.
    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
        fs::write(
            dir.path().join(".infigraph/config.toml"),
            "[ignore]\ncode = [\"docs/\"]\ndocs = [\"src/\"]\n",
        )
        .unwrap();
        dir
    }

    /// Offers `path` to every domain of `triage` and delivers each routed
    /// batch to the fake of its domain, as the producer does.
    fn dispatch(triage: &Triage, sinks: &mut [FakeSink], kind: &EventKind, path: &Path) {
        let path = triage.respelled(path).into_owned();
        for (domain, batch) in triage.route(kind, &path) {
            for sink in sinks.iter_mut().filter(|s| s.domain() == domain) {
                sink.accept(batch.clone());
            }
        }
    }

    #[test]
    fn a_path_reaches_only_the_sinks_whose_domain_keeps_it() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        let code_file = write(dir.path(), "src/main.rs");
        let doc_file = write(dir.path(), "docs/guide.md");
        let triage = Triage::new(dir.path(), &[Domain::Code, Domain::Docs]);
        let (code, code_seen) = FakeSink::new(Domain::Code);
        let (docs, docs_seen) = FakeSink::new(Domain::Docs);
        let mut sinks = [code, docs];

        dispatch(&triage, &mut sinks, &create(), &code_file);
        dispatch(&triage, &mut sinks, &create(), &doc_file);

        let code_seen = code_seen.lock().unwrap();
        let docs_seen = docs_seen.lock().unwrap();
        assert_eq!(
            code_seen
                .iter()
                .flat_map(|b| &b.changes)
                .collect::<Vec<_>>(),
            vec![&(PathBuf::from("src/main.rs"), PathChange::Created)],
            "the code sink gets the code path and not the docs-only one"
        );
        assert_eq!(
            docs_seen
                .iter()
                .flat_map(|b| &b.changes)
                .collect::<Vec<_>>(),
            vec![&(PathBuf::from("docs/guide.md"), PathChange::Created)],
            "the docs sink gets the docs path and not the code-only one"
        );
    }

    #[test]
    fn an_ignored_or_hidden_path_reaches_no_sink() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        fs::write(dir.path().join(".gitignore"), "generated/\n").unwrap();
        let ignored = write(dir.path(), "generated/out.rs");
        let vendored = write(dir.path(), "node_modules/x/index.js");
        let hidden = write(dir.path(), ".hidden/a.rs");
        let triage = Triage::new(dir.path(), &[Domain::Code, Domain::Docs]);
        let (code, code_seen) = FakeSink::new(Domain::Code);
        let (docs, docs_seen) = FakeSink::new(Domain::Docs);
        let mut sinks = [code, docs];

        for path in [&ignored, &vendored, &hidden] {
            dispatch(&triage, &mut sinks, &modify(), path);
        }

        assert!(code_seen.lock().unwrap().is_empty());
        assert!(docs_seen.lock().unwrap().is_empty());
    }

    #[test]
    fn the_routing_agrees_with_the_walk_for_each_domain() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        fs::write(dir.path().join(".gitignore"), "generated/\n").unwrap();
        for rel in [
            "src/main.rs",
            "docs/guide.md",
            "README.md",
            "generated/out.rs",
            ".hidden/a.rs",
            "target/debug/x.rs",
        ] {
            write(dir.path(), rel);
        }
        let triage = Triage::new(dir.path(), &[Domain::Code, Domain::Docs]);

        for domain in [Domain::Code, Domain::Docs] {
            let walked: std::collections::BTreeSet<String> =
                crate::ignore_rules::walk_builder(dir.path(), domain)
                    .build()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
                    .map(|e| {
                        e.path()
                            .strip_prefix(dir.path())
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/")
                    })
                    .filter(|rel| !rel.starts_with(".infigraph/"))
                    .collect();
            let routed: std::collections::BTreeSet<String> = [
                "src/main.rs",
                "docs/guide.md",
                "README.md",
                "generated/out.rs",
                ".hidden/a.rs",
                "target/debug/x.rs",
            ]
            .iter()
            .filter(|rel| {
                triage
                    .route(&create(), &dir.path().join(rel))
                    .iter()
                    .any(|(d, _)| *d == domain)
            })
            .map(|s| s.to_string())
            .collect();
            assert_eq!(routed, walked, "{domain:?}: routing and the walk disagree");
        }
    }

    #[test]
    fn a_directory_that_merely_changed_reaches_no_sink() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        write(dir.path(), "notes/a.md");
        let triage = Triage::new(dir.path(), &[Domain::Code, Domain::Docs]);

        assert!(triage
            .route(&modify(), &dir.path().join("notes"))
            .is_empty());
        assert!(triage.route(&modify(), dir.path()).is_empty());
    }

    #[test]
    fn a_removal_reaches_the_sink_whether_it_named_a_file_or_a_directory() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        let triage = Triage::new(dir.path(), &[Domain::Code]);
        let kind = EventKind::Remove(RemoveKind::Any);

        // Neither path exists any more, so nothing says which was a directory.
        for rel in ["src/gone.rs", "src/gone_dir"] {
            let routed = triage.route(&kind, &dir.path().join(rel));
            assert_eq!(
                routed,
                vec![(
                    Domain::Code,
                    WatchBatch {
                        changes: vec![(PathBuf::from(rel), PathChange::Removed)],
                        rescan: false
                    }
                )],
                "{rel}"
            );
        }
    }

    #[test]
    fn a_domain_attached_later_judges_later_paths_only_and_a_detached_one_none() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        let file = write(dir.path(), "README.md");
        let mut triage = Triage::new(dir.path(), &[Domain::Code]);
        assert_eq!(triage.domains(), vec![Domain::Code]);
        assert!(!triage
            .route(&create(), &file)
            .iter()
            .any(|(d, _)| *d == Domain::Docs));

        triage.attach(Domain::Docs);
        assert!(triage
            .route(&create(), &file)
            .iter()
            .any(|(d, _)| *d == Domain::Docs));

        triage.detach(Domain::Docs);
        assert!(!triage
            .route(&create(), &file)
            .iter()
            .any(|(d, _)| *d == Domain::Docs));
    }
}
