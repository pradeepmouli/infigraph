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
    expansion_cap: usize,
}

/// Files a single arriving directory is expanded into before the sink is told
/// to rescan instead: a `git checkout` that moves a whole tree in is a rescan's
/// job, not a list of tens of thousands of paths.
const EXPANSION_CAP: usize = 20_000;

impl Triage {
    pub fn new(root: &Path, domains: &[Domain]) -> Self {
        let mut triage = Self {
            root: root.to_path_buf(),
            matchers: Vec::new(),
            expansion_cap: EXPANSION_CAP,
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

    /// Whether any attached domain's walk would visit `path`.
    pub fn keeps(&self, path: &Path, is_dir: bool) -> bool {
        self.matchers.iter().any(|(_, m)| !m.skips(path, is_dir))
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
                // A directory that merely changed says nothing. One that
                // arrived (created, or renamed into place) may hold files that
                // never get an event of their own: inotify registers a new
                // directory only after the files in it were written, and a
                // directory moved in carries its children with it.
                if directory_arrived(kind) {
                    routed.push((*domain, self.expand(path, *domain)));
                }
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

/// The triage and the sinks it feeds: everything the producer does with an
/// event path after it has been respelled and checked against the canary.
pub struct Dispatcher {
    triage: Triage,
    sinks: Vec<Box<dyn WatchSink>>,
}

/// A change to the set of sinks, requested from outside the producer.
pub enum SinkCommand {
    Attach(Box<dyn WatchSink>),
    Detach(&'static str),
}

static COMMANDS: std::sync::Mutex<Vec<(PathBuf, SinkCommand)>> = std::sync::Mutex::new(Vec::new());

/// Asks the producer watching `root` to attach `sink` at its next tick. A
/// producer that starts later finds the request waiting.
pub fn request_attach(root: &Path, sink: Box<dyn WatchSink>) {
    let root = crate::project::canonicalize_lenient(root);
    COMMANDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((root, SinkCommand::Attach(sink)));
}

/// Asks the producer watching `root` to detach the sink called `name`.
pub fn request_detach(root: &Path, name: &'static str) {
    let root = crate::project::canonicalize_lenient(root);
    COMMANDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((root, SinkCommand::Detach(name)));
}

fn take_commands(root: &Path) -> Vec<SinkCommand> {
    let root = crate::project::canonicalize_lenient(root);
    let mut all = COMMANDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *all)
        .into_iter()
        .partition(|(r, _)| *r == root);
    *all = rest;
    mine.into_iter().map(|(_, c)| c).collect()
}

impl Dispatcher {
    pub fn new(root: &Path, sinks: Vec<Box<dyn WatchSink>>) -> Self {
        let mut domains: Vec<Domain> = Vec::new();
        for sink in &sinks {
            if !domains.contains(&sink.domain()) {
                domains.push(sink.domain());
            }
        }
        Self {
            triage: Triage::new(root, &domains),
            sinks,
        }
    }

    pub fn triage(&self) -> &Triage {
        &self.triage
    }

    pub fn triage_mut(&mut self) -> &mut Triage {
        &mut self.triage
    }

    /// Adds a sink. It is told to rescan at once: it was not listening, so
    /// whatever changed before is not in any batch it will see.
    pub fn attach(&mut self, mut sink: Box<dyn WatchSink>) {
        self.triage.attach(sink.domain());
        sink.accept(WatchBatch::rescan());
        self.sinks.push(sink);
    }

    /// Removes the sinks called `name`, and stops judging a domain nobody
    /// listens to any more.
    pub fn detach(&mut self, name: &str) {
        self.sinks.retain(|s| s.name() != name);
        for domain in self.triage.domains() {
            if !self.sinks.iter().any(|s| s.domain() == domain) {
                self.triage.detach(domain);
            }
        }
    }

    /// Applies the attach and detach requests made for `root` since the last
    /// call.
    pub fn apply_requests(&mut self, root: &Path) {
        for command in take_commands(root) {
            match command {
                SinkCommand::Attach(sink) => self.attach(sink),
                SinkCommand::Detach(name) => self.detach(name),
            }
        }
    }

    /// Offers one event path (already respelled) to every sink whose domain
    /// keeps it.
    pub fn dispatch(&mut self, kind: &EventKind, path: &Path) {
        for (domain, batch) in self.triage.route(kind, path) {
            for sink in self.sinks.iter_mut().filter(|s| s.domain() == domain) {
                sink.accept(batch.clone());
            }
        }
    }

    /// Tells every sink events were lost.
    pub fn rescan_all(&mut self) {
        for sink in &mut self.sinks {
            sink.accept(WatchBatch::rescan());
        }
    }

    pub fn tick(&mut self) {
        for sink in &mut self.sinks {
            sink.tick();
        }
    }

    pub fn health(&self) -> Vec<(&'static str, SinkHealth)> {
        self.sinks.iter().map(|s| (s.name(), s.health())).collect()
    }
}

impl Triage {
    /// The files below an arriving directory that `domain`'s walk visits, each
    /// as `Created`. More than the cap, or a walk error, makes it a rescan:
    /// the list would be short.
    fn expand(&self, dir: &Path, domain: Domain) -> WatchBatch {
        let mut batch = WatchBatch::default();
        // Judged by the project the matchers were built for, not by whatever
        // project a bare subdirectory resolves to.
        let project_root = crate::project::resolve_project_root(&self.root);
        for entry in crate::ignore_rules::walk_builder_in(dir, &project_root, domain).build() {
            let Ok(entry) = entry else {
                batch.rescan = true;
                continue;
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            if batch.changes.len() >= self.expansion_cap {
                return WatchBatch::rescan();
            }
            if let Ok(rel) = entry.path().strip_prefix(&self.root) {
                batch.changes.push((
                    PathBuf::from(rel.to_string_lossy().replace('\\', "/")),
                    PathChange::Created,
                ));
            }
        }
        batch
    }

    /// For tests: a smaller cap than [`EXPANSION_CAP`].
    #[cfg(test)]
    pub(crate) fn with_expansion_cap(mut self, cap: usize) -> Self {
        self.expansion_cap = cap;
        self
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
    use std::sync::{Arc, Mutex};

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

    /// A dispatcher over fakes of the given domains, and what each recorded.
    fn dispatcher(
        root: &Path,
        domains: &[Domain],
    ) -> (Dispatcher, Vec<Arc<Mutex<Vec<WatchBatch>>>>) {
        let mut seen = Vec::new();
        let mut sinks: Vec<Box<dyn WatchSink>> = Vec::new();
        for domain in domains {
            let (sink, record) = FakeSink::new(*domain);
            seen.push(record);
            sinks.push(Box::new(sink));
        }
        (Dispatcher::new(root, sinks), seen)
    }

    /// Delivers one event the way the producer does: respelled, then offered.
    fn deliver(dispatcher: &mut Dispatcher, kind: &EventKind, path: &Path) {
        let path = dispatcher.triage().respelled(path).into_owned();
        dispatcher.dispatch(kind, &path);
    }

    #[test]
    fn a_path_reaches_only_the_sinks_whose_domain_keeps_it() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        let code_file = write(dir.path(), "src/main.rs");
        let doc_file = write(dir.path(), "docs/guide.md");
        let (mut d, seen) = dispatcher(dir.path(), &[Domain::Code, Domain::Docs]);

        deliver(&mut d, &create(), &code_file);
        deliver(&mut d, &create(), &doc_file);

        let code_seen = seen[0].lock().unwrap();
        let docs_seen = seen[1].lock().unwrap();
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
        let (mut d, seen) = dispatcher(dir.path(), &[Domain::Code, Domain::Docs]);

        for path in [&ignored, &vendored, &hidden] {
            deliver(&mut d, &modify(), path);
        }

        assert!(seen[0].lock().unwrap().is_empty());
        assert!(seen[1].lock().unwrap().is_empty());
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

    fn files_of(batch: &WatchBatch) -> Vec<String> {
        let mut files: Vec<String> = batch
            .changes
            .iter()
            .map(|(p, c)| {
                assert_eq!(*c, PathChange::Created);
                p.to_string_lossy().into_owned()
            })
            .collect();
        files.sort();
        files
    }

    fn renamed_in() -> EventKind {
        EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Any))
    }

    #[test]
    fn a_directory_moved_in_with_files_already_inside_yields_those_files() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        write(dir.path(), "moved/a.md");
        write(dir.path(), "moved/deep/b.md");
        write(dir.path(), "moved/deep/c.rs");
        let triage = Triage::new(dir.path(), &[Domain::Code, Domain::Docs]);

        let routed = triage.route(&renamed_in(), &dir.path().join("moved"));

        for (domain, batch) in &routed {
            assert_eq!(
                files_of(batch),
                vec!["moved/a.md", "moved/deep/b.md", "moved/deep/c.rs"],
                "{domain:?}: every file inside, none of them with an event of its own"
            );
            assert!(!batch.rescan);
        }
        assert_eq!(routed.len(), 2);
    }

    #[test]
    fn a_new_directory_holding_a_doc_routes_the_doc_by_domain() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        // `docs/` is out of the code walk and `src/` out of the docs walk.
        write(dir.path(), "pkg/docs/guide.md");
        write(dir.path(), "pkg/src/lib.rs");
        let triage = Triage::new(dir.path(), &[Domain::Code, Domain::Docs]);

        let routed = triage.route(&create(), &dir.path().join("pkg"));
        let of = |wanted: Domain| {
            routed
                .iter()
                .find(|(d, _)| *d == wanted)
                .map(|(_, b)| files_of(b))
                .unwrap()
        };

        assert_eq!(of(Domain::Code), vec!["pkg/src/lib.rs"]);
        assert_eq!(of(Domain::Docs), vec!["pkg/docs/guide.md"]);
    }

    #[test]
    fn an_arriving_directory_expands_only_what_the_walk_visits() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        fs::write(dir.path().join(".gitignore"), "skipped/\n").unwrap();
        write(dir.path(), "new/keep.md");
        write(dir.path(), "new/skipped/no.md");
        write(dir.path(), "new/node_modules/x.md");
        write(dir.path(), "new/.hidden/h.md");
        let triage = Triage::new(dir.path(), &[Domain::Docs]);

        let routed = triage.route(&create(), &dir.path().join("new"));

        assert_eq!(files_of(&routed[0].1), vec!["new/keep.md"]);
    }

    #[test]
    fn a_directory_past_the_cap_asks_for_a_rescan_instead_of_a_short_list() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        for i in 0..5 {
            write(dir.path(), &format!("big/f{i}.md"));
        }
        let triage = Triage::new(dir.path(), &[Domain::Docs]).with_expansion_cap(3);

        let routed = triage.route(&create(), &dir.path().join("big"));

        assert_eq!(routed.len(), 1);
        assert!(routed[0].1.rescan);
        assert!(routed[0].1.changes.is_empty());
    }

    #[test]
    fn a_sink_attached_mid_run_gets_a_rescan_then_later_batches_and_a_detached_one_none() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        let before = write(dir.path(), "README.md");
        let after = write(dir.path(), "GUIDE.md");
        let (mut d, code_seen) = dispatcher(dir.path(), &[Domain::Code]);
        deliver(&mut d, &create(), &before);

        let (docs, docs_seen) = FakeSink::new(Domain::Docs);
        d.attach(Box::new(docs));
        deliver(&mut d, &create(), &after);

        let seen_after_attach = docs_seen.lock().unwrap().clone();
        assert!(
            seen_after_attach[0].rescan,
            "it was not listening before: {seen_after_attach:?}"
        );
        assert_eq!(
            seen_after_attach[1..]
                .iter()
                .flat_map(|b| b
                    .changes
                    .iter()
                    .map(|(p, _)| p.to_string_lossy().into_owned()))
                .collect::<Vec<_>>(),
            ["GUIDE.md"],
            "later batches only, never the one from before it attached"
        );
        assert_eq!(code_seen[0].lock().unwrap().len(), 2);

        d.detach("fake");
        deliver(&mut d, &create(), &write(dir.path(), "MORE.md"));
        assert_eq!(
            docs_seen.lock().unwrap().len(),
            seen_after_attach.len(),
            "a detached sink hears nothing more"
        );
        assert!(
            !d.triage().domains().contains(&Domain::Docs),
            "and its domain is no longer judged"
        );
    }

    #[test]
    fn requests_made_from_outside_are_applied_to_the_producers_own_dispatcher() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project();
        let (mut d, _) = dispatcher(dir.path(), &[Domain::Code]);
        let (docs, _) = FakeSink::new(Domain::Docs);

        request_attach(dir.path(), Box::new(docs));
        d.apply_requests(dir.path());
        assert_eq!(d.health().len(), 2);

        request_detach(dir.path(), "fake");
        d.apply_requests(dir.path());
        assert!(d.health().is_empty());
    }
}
