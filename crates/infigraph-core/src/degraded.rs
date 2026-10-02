//! Degraded modes: the ways infigraph carries on with less than it should,
//! and the one place that names them (#75).
//!
//! A fallback nobody can see is a bug with extra steps. Before this module
//! each fallback wrote its own line to whatever stderr it had, which for the
//! daemon is `.infigraph/daemon.log`, and the wording lived at each site.
//! Here the set of modes is one enum with one wording each, and [`gather`]
//! answers "what is degraded for this project right now" for every surface:
//! `infigraph doctor`, the MCP `get_stats` tool and the tool footers.
//!
//! `gather` is ground truth, not memory (#78): it re-derives what it can from
//! files on disk each time it is asked. It never starts a daemon, never takes
//! a lease, never opens a store and never creates `.infigraph/`.
//!
//! A new fallback must be added here, or it is invisible.

use std::path::Path;

use serde::{Deserialize, Serialize};

pub const EMBEDDINGS_STALE: &str = "embeddings-stale";
pub const UNWATCHED_DIRECTORIES: &str = "unwatched-directories";
pub const DOC_READS_UNAVAILABLE: &str = "doc-reads-unavailable";
pub const GRAPH_REOPEN_BACKOFF: &str = "graph-reopen-backoff";

/// One way infigraph is running degraded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DegradedMode {
    /// This process could not load the Model2Vec model, so it embeds with
    /// trigram hashing.
    TrigramEmbedder,
    /// `embeddings.bin` was built by the trigram embedder (its marker says
    /// so), whatever this process embeds with.
    EmbeddingsBuiltWithTrigram,
    /// `embeddings.bin` holds vectors from more than one embedder.
    EmbeddingsMixed,
    /// This process embeds queries with a different embedder than the one
    /// that built `embeddings.bin`, so it compares unrelated vectors.
    EmbedderMismatch { built: String, query: String },
    /// The graph exists but `embeddings.bin` does not, so code search embeds
    /// every symbol at query time.
    EmbeddingsMissing,
    /// `embeddings.bin` is older than the graph it was built from, judged by
    /// file times: the rule a process without the graph open can apply.
    EmbeddingsStale { minutes: u64 },
    /// `embeddings.bin` reflects an older graph generation than the live one.
    /// Only the process that holds the graph (the daemon) can say so, and
    /// its answer replaces the file-time guess.
    EmbeddingsBehindGraph { recorded: i64, current: i64 },
    /// The watcher could not watch some directories, so changes under them
    /// go unnoticed.
    UnwatchedDirectories { failed: usize, first: String },
    /// The daemon cannot open the document store, so it serves code only.
    DocReadsUnavailable { reason: String },
    /// The daemon cannot reopen the graph and is backing off; writes wait.
    GraphReopenBackoff { failures: u32, retry_secs: u64 },
    /// The project is past the HNSW threshold but the index (or its `.meta`)
    /// is absent, so vector search is a linear scan.
    HnswMissing,
    /// Documents are indexed but their embeddings are absent, so document
    /// search ranks by keywords only.
    DocEmbeddingsMissing,
}

impl DegradedMode {
    /// A stable identifier, for machine readers and for telling modes apart
    /// across builds.
    pub fn key(&self) -> &'static str {
        match self {
            DegradedMode::TrigramEmbedder => "trigram-embedder",
            DegradedMode::EmbeddingsBuiltWithTrigram => "embeddings-built-with-trigram",
            DegradedMode::EmbeddingsMixed => "embeddings-mixed",
            DegradedMode::EmbedderMismatch { .. } => "embedder-mismatch",
            DegradedMode::EmbeddingsMissing => "embeddings-missing",
            DegradedMode::EmbeddingsStale { .. } | DegradedMode::EmbeddingsBehindGraph { .. } => {
                EMBEDDINGS_STALE
            }
            DegradedMode::UnwatchedDirectories { .. } => UNWATCHED_DIRECTORIES,
            DegradedMode::DocReadsUnavailable { .. } => DOC_READS_UNAVAILABLE,
            DegradedMode::GraphReopenBackoff { .. } => GRAPH_REOPEN_BACKOFF,
            DegradedMode::HnswMissing => "hnsw-missing",
            DegradedMode::DocEmbeddingsMissing => "doc-embeddings-missing",
        }
    }

    /// What is degraded, in one line. The only wording for this mode.
    pub fn message(&self) -> String {
        match self {
            DegradedMode::TrigramEmbedder => {
                "semantic search degraded: Model2Vec model unavailable, using trigram fallback"
                    .to_string()
            }
            DegradedMode::EmbeddingsBuiltWithTrigram => {
                "embeddings.bin was built with the trigram fallback, not the Model2Vec model: \
                 semantic ranking is degraded"
                    .to_string()
            }
            DegradedMode::EmbeddingsMixed => {
                "embeddings.bin holds vectors from more than one embedder: semantic ranking \
                 compares unrelated vectors"
                    .to_string()
            }
            DegradedMode::EmbedderMismatch { built, query } => format!(
                "queries are embedded with {query} but embeddings.bin was built with {built}: \
                 semantic ranking compares unrelated vectors"
            ),
            DegradedMode::EmbeddingsMissing => {
                "embeddings.bin is missing: code search embeds every symbol at query time"
                    .to_string()
            }
            DegradedMode::EmbeddingsStale { minutes } => format!(
                "embeddings.bin is {minutes} minutes older than the graph: semantic ranking may \
                 be stale"
            ),
            DegradedMode::EmbeddingsBehindGraph { recorded, current } => format!(
                "embeddings.bin was built from graph generation {recorded}, but the graph is \
                 now at generation {current}: semantic ranking is stale"
            ),
            DegradedMode::UnwatchedDirectories { failed, first } => format!(
                "{failed} director(ies) could not be watched (first: {first}): changes under \
                 them will not be noticed"
            ),
            DegradedMode::DocReadsUnavailable { reason } => {
                format!("document reads are unavailable, the daemon serves code only: {reason}")
            }
            DegradedMode::GraphReopenBackoff {
                failures,
                retry_secs,
            } => format!(
                "the daemon cannot reopen the graph ({failures} consecutive failures, next \
                 attempt in {retry_secs}s): writes are waiting"
            ),
            DegradedMode::HnswMissing => {
                "HNSW index missing — vector search is on a linear scan this project has \
                 outgrown"
                    .to_string()
            }
            DegradedMode::DocEmbeddingsMissing => {
                "document embeddings are missing: document search ranks by keywords only"
                    .to_string()
            }
        }
    }

    /// What to do about it.
    pub fn remedy(&self) -> &'static str {
        match self {
            DegradedMode::TrigramEmbedder => {
                "run `infigraph install` to install the model, or set INFIGRAPH_MODEL_DIR"
            }
            DegradedMode::EmbeddingsBuiltWithTrigram
            | DegradedMode::EmbeddingsMixed
            | DegradedMode::EmbedderMismatch { .. } => {
                "install the model (`infigraph install`), then run `infigraph index --full`"
            }
            DegradedMode::EmbeddingsMissing
            | DegradedMode::EmbeddingsStale { .. }
            | DegradedMode::EmbeddingsBehindGraph { .. }
            | DegradedMode::HnswMissing => "run `infigraph index` to rebuild them",
            DegradedMode::UnwatchedDirectories { .. } => {
                "fix the cause (on Linux usually `fs.inotify.max_user_watches`), then run \
                 `infigraph daemon-restart`"
            }
            DegradedMode::DocReadsUnavailable { .. } => {
                "run `infigraph reindex-docs`, then `infigraph daemon-restart`"
            }
            DegradedMode::GraphReopenBackoff { .. } => {
                "see .infigraph/daemon.log for the holder; `infigraph doctor` names it"
            }
            DegradedMode::DocEmbeddingsMissing => "run `infigraph index-docs` to rebuild them",
        }
    }

    /// The form that travels and is rendered.
    pub fn notice(&self) -> Notice {
        Notice {
            key: self.key().to_string(),
            message: self.message(),
            remedy: self.remedy().to_string(),
        }
    }
}

/// A degraded mode as every surface renders it, and as it crosses a process
/// boundary. Plain strings on purpose: a daemon from a newer build can report
/// a mode this build has no variant for, and it must still be shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub key: String,
    pub message: String,
    pub remedy: String,
}

/// Every degraded mode in effect for the project at `root`, as seen from this
/// process. Empty for a project with no index, and in remote mode, where the
/// sidecars this reads do not exist.
///
/// Three sources, merged by key: what files on disk establish, what this
/// process knows about itself, and what the project's live daemon reports
/// over the status socket. Asking the daemon takes no lease and starts none;
/// with no daemon its part is simply absent.
pub fn gather(root: &Path) -> Vec<Notice> {
    let mut modes = Vec::new();
    if crate::embed::trigram_fallback_active() {
        modes.push(DegradedMode::TrigramEmbedder);
    }
    modes.extend(embedder_modes(root, crate::embed::process_embedder()));
    modes.extend(derived_from_disk(root));
    let mut local: Vec<Notice> = modes.iter().map(DegradedMode::notice).collect();
    local = merge(local, live::for_root(root));

    // The daemon's own process already has its list in `live`.
    let from_daemon = if crate::daemon::lease::is_self_daemon(root) {
        None
    } else {
        crate::daemon::control::query_status(root)
            .ok()
            .map(|report| report.degraded)
    };
    merge_with_daemon(local, from_daemon)
}

/// `local` with the daemon's answer folded in. A daemon that answered knows
/// the graph's generation, so its word on staleness is final either way: its
/// notice replaces the local file-time guess, and its silence removes it.
/// `None` is "no daemon answered", and the local list stands.
pub fn merge_with_daemon(local: Vec<Notice>, from_daemon: Option<Vec<Notice>>) -> Vec<Notice> {
    let Some(from_daemon) = from_daemon else {
        return local;
    };
    let local = local
        .into_iter()
        .filter(|n| n.key != EMBEDDINGS_STALE)
        .collect();
    merge(local, from_daemon)
}

/// `base` with `over` folded in by key: a notice in `over` replaces the one
/// with its key in `base` (in place), and the rest of `over` follows.
pub fn merge(mut base: Vec<Notice>, over: Vec<Notice>) -> Vec<Notice> {
    for notice in over {
        match base.iter_mut().find(|n| n.key == notice.key) {
            Some(slot) => *slot = notice,
            None => base.push(notice),
        }
    }
    base
}

/// What a live process reports about itself: modes that exist only while it
/// runs and that no file records (its watcher's failed registrations, a
/// store it could not open, a reopen it is backing off from). The daemon's
/// list travels in `StatusReport::degraded`. Nothing here outlives the
/// process, by design: a restart re-registers, reopens and recomputes, so
/// there is no stale entry to trust.
pub mod live {
    use super::{DegradedMode, Notice, EMBEDDINGS_STALE};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static LIVE: Mutex<BTreeMap<(PathBuf, String), Notice>> = Mutex::new(BTreeMap::new());

    fn key(root: &Path) -> PathBuf {
        crate::project::canonicalize_lenient(root)
    }

    fn lock() -> std::sync::MutexGuard<'static, BTreeMap<(PathBuf, String), Notice>> {
        LIVE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record `mode` for `root`, replacing an earlier report of the same key.
    pub fn set(root: &Path, mode: DegradedMode) {
        lock().insert((key(root), mode.key().to_string()), mode.notice());
    }

    /// The mode with `mode_key` no longer holds for `root`.
    pub fn clear(root: &Path, mode_key: &str) {
        lock().remove(&(key(root), mode_key.to_string()));
    }

    pub fn for_root(root: &Path) -> Vec<Notice> {
        let root = key(root);
        lock()
            .iter()
            .filter(|((r, _), _)| *r == root)
            .map(|(_, n)| n.clone())
            .collect()
    }

    /// Everything this process reports, for every root. A daemon serves one
    /// project, so for it this is that project's list.
    pub fn all() -> Vec<Notice> {
        lock().values().cloned().collect()
    }

    /// Set or clear "document embeddings are missing" for `root`, from the
    /// number of chunks the document store holds. Called by the document
    /// indexer, which has the store open: with no chunks there is nothing to
    /// embed, and a missing file is then not a degradation.
    pub fn note_doc_embeddings(root: &Path, chunks_in_store: usize) {
        let present = root.join(".infigraph").join("docs_embeddings.bin").exists();
        if chunks_in_store > 0 && !present {
            set(root, DegradedMode::DocEmbeddingsMissing);
        } else {
            clear(root, DegradedMode::DocEmbeddingsMissing.key());
        }
    }

    /// Compare the generation `embeddings.bin` was built from with the graph's
    /// `current` one, and set or clear the staleness mode. Called by whoever
    /// holds the graph, after it tried to update the embeddings. No marker
    /// means it cannot be judged, which is not reported.
    pub fn note_embeddings_generation(root: &Path, current: i64) {
        let sidecar = root.join(".infigraph").join("embeddings.bin");
        match crate::embed::read_generation_marker(&sidecar) {
            Some(recorded) if current > 0 && recorded < current => set(
                root,
                DegradedMode::EmbeddingsBehindGraph { recorded, current },
            ),
            _ => clear(root, EMBEDDINGS_STALE),
        }
    }
}

/// What the embedder marker beside `embeddings.bin` establishes, given the
/// embedder this process queries with (`None` if it has built none yet). An
/// absent marker is unknown and reports nothing.
pub fn embedder_modes(root: &Path, process_embedder: Option<&str>) -> Vec<DegradedMode> {
    let mut modes = Vec::new();
    let sidecar = root.join(".infigraph").join("embeddings.bin");
    if crate::daemon::lifecycle::is_remote_backend() || !sidecar.exists() {
        return modes;
    }
    let Some(built) = crate::embed::read_embedder_marker(&sidecar) else {
        return modes;
    };
    if built == crate::embed::MIXED_EMBEDDERS {
        modes.push(DegradedMode::EmbeddingsMixed);
        return modes;
    }
    if built == crate::embed::TRIGRAM_EMBEDDER {
        modes.push(DegradedMode::EmbeddingsBuiltWithTrigram);
    }
    if let Some(query) = process_embedder {
        if query != built {
            modes.push(DegradedMode::EmbedderMismatch {
                built,
                query: query.to_string(),
            });
        }
    }
    modes
}

/// The modes that files on disk alone establish. No process state, no store.
pub fn derived_from_disk(root: &Path) -> Vec<DegradedMode> {
    let mut modes = Vec::new();
    let ig = root.join(".infigraph");
    if crate::daemon::lifecycle::is_remote_backend() || !ig.join("graph").exists() {
        return modes;
    }

    if !ig.join("embeddings.bin").exists() {
        modes.push(DegradedMode::EmbeddingsMissing);
    } else if let Some(lag) = crate::doctor::sidecar_lag(root, "embeddings.bin") {
        if lag.as_secs() > crate::doctor::SIDECAR_STALE_SECS {
            modes.push(DegradedMode::EmbeddingsStale {
                minutes: lag.as_secs() / 60,
            });
        }
    }
    if crate::embed::hnsw_expected_but_missing(root) {
        modes.push(DegradedMode::HnswMissing);
    }
    // Not here: missing document embeddings. An empty document index has no
    // `docs_embeddings.bin` either (measured), and telling the two apart
    // needs the chunk count, which only the indexer has. It reports the mode
    // through `live::note_doc_embeddings`.
    modes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};
    use std::time::{Duration, SystemTime};

    /// A project with a graph file, and `HOME` and the docs switch pinned so a
    /// developer's own config cannot turn documents on.
    struct Project {
        dir: tempfile::TempDir,
        _home: PinnedHome,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Project {
        fn new() -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::remove_var("INFIGRAPH_DOCS_ENABLED");
            let home = PinnedHome::empty();
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
            let project = Self {
                dir,
                _home: home,
                _guard: guard,
            };
            project.write("graph", b"graph");
            project
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, name: &str, bytes: &[u8]) {
            std::fs::write(self.root().join(".infigraph").join(name), bytes).unwrap();
        }

        /// An `embeddings.bin` whose header claims `count` vectors.
        fn embeddings(&self, count: u32) {
            self.write("embeddings.bin", &count.to_le_bytes());
        }

        fn age(&self, name: &str, by: Duration) {
            let file = std::fs::File::options()
                .write(true)
                .open(self.root().join(".infigraph").join(name))
                .unwrap();
            file.set_modified(SystemTime::now() - by).unwrap();
        }

        fn enable_docs(&self) {
            self.write("config.toml", b"[docs]\nenabled = true\n");
            self.write("docs.kuzu", b"docs");
        }
    }

    #[test]
    fn a_project_with_fresh_sidecars_is_not_degraded() {
        let p = Project::new();
        p.embeddings(10);
        assert_eq!(derived_from_disk(p.root()), vec![]);
    }

    #[test]
    fn a_directory_with_no_index_reports_nothing_and_gets_nothing_created() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(derived_from_disk(dir.path()), vec![]);
        assert!(gather(dir.path())
            .iter()
            .all(|n| n.key == DegradedMode::TrigramEmbedder.key()));
        assert!(
            !dir.path().join(".infigraph").exists(),
            "asking what is degraded created .infigraph/"
        );
    }

    #[test]
    fn a_graph_without_embeddings_is_reported() {
        let p = Project::new();
        assert_eq!(
            derived_from_disk(p.root()),
            vec![DegradedMode::EmbeddingsMissing]
        );
    }

    #[test]
    fn embeddings_older_than_the_graph_are_reported_as_stale() {
        let p = Project::new();
        p.embeddings(10);
        p.age("embeddings.bin", Duration::from_secs(3 * 60 * 60));
        // The graph was written a moment before the sidecar was aged, so
        // the lag is just under three hours.
        match derived_from_disk(p.root()).as_slice() {
            [DegradedMode::EmbeddingsStale { minutes }] => {
                assert!((179..=180).contains(minutes), "{minutes}")
            }
            other => panic!("expected one stale mode, got {other:?}"),
        }
        // A little behind is not stale.
        p.age("embeddings.bin", Duration::from_secs(10 * 60));
        assert_eq!(derived_from_disk(p.root()), vec![]);
    }

    #[test]
    fn a_missing_hnsw_index_is_reported_only_past_the_threshold() {
        let p = Project::new();
        p.embeddings(1_000);
        assert_eq!(derived_from_disk(p.root()), vec![]);

        p.embeddings(crate::embed::HNSW_THRESHOLD as u32);
        assert_eq!(derived_from_disk(p.root()), vec![DegradedMode::HnswMissing]);

        // The index without its `.meta` is as unusable as no index:
        // `search_hnsw` answers `None` for either.
        p.write("hnsw_index.usearch", b"stub");
        assert_eq!(derived_from_disk(p.root()), vec![DegradedMode::HnswMissing]);

        p.write("hnsw_index.meta", b"stub");
        assert_eq!(derived_from_disk(p.root()), vec![]);
    }

    /// An empty document index has no `docs_embeddings.bin` either, so files
    /// alone cannot say the embeddings are missing: the indexer reports it,
    /// with the chunk count it has.
    #[test]
    fn missing_document_embeddings_are_reported_only_when_there_are_chunks() {
        let p = Project::new();
        p.embeddings(10);
        p.enable_docs();
        assert_eq!(derived_from_disk(p.root()), vec![], "files cannot tell");

        live::note_doc_embeddings(p.root(), 0);
        assert_eq!(
            live::for_root(p.root()),
            vec![],
            "an empty index is healthy"
        );

        live::note_doc_embeddings(p.root(), 12);
        let notices = live::for_root(p.root());
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].key, DegradedMode::DocEmbeddingsMissing.key());

        p.write("docs_embeddings.bin", &1u32.to_le_bytes());
        live::note_doc_embeddings(p.root(), 12);
        assert_eq!(live::for_root(p.root()), vec![]);
    }

    #[test]
    fn every_mode_has_its_own_key_and_wording() {
        let modes = [
            DegradedMode::EmbeddingsBuiltWithTrigram,
            DegradedMode::EmbeddingsMixed,
            DegradedMode::EmbedderMismatch {
                built: "model2vec".to_string(),
                query: "trigram".to_string(),
            },
            DegradedMode::TrigramEmbedder,
            DegradedMode::UnwatchedDirectories {
                failed: 2,
                first: "x".to_string(),
            },
            DegradedMode::DocReadsUnavailable {
                reason: "r".to_string(),
            },
            DegradedMode::GraphReopenBackoff {
                failures: 3,
                retry_secs: 20,
            },
            DegradedMode::EmbeddingsMissing,
            DegradedMode::EmbeddingsStale { minutes: 90 },
            DegradedMode::HnswMissing,
            DegradedMode::DocEmbeddingsMissing,
        ];
        let keys: std::collections::HashSet<_> = modes.iter().map(|m| m.key()).collect();
        let messages: std::collections::HashSet<_> = modes.iter().map(|m| m.message()).collect();
        assert_eq!(keys.len(), modes.len());
        assert_eq!(messages.len(), modes.len());
        for mode in &modes {
            let notice = mode.notice();
            assert_eq!(notice.key, mode.key());
            assert!(!notice.message.is_empty() && !notice.remedy.is_empty());
        }
    }

    fn marker(p: &Project, name: &str) {
        crate::embed::write_embedder_marker(&p.root().join(".infigraph/embeddings.bin"), name)
            .unwrap();
    }

    /// Read from disk alone: this is what a fresh process sees after the
    /// daemon that indexed with the fallback has gone.
    #[test]
    fn embeddings_built_with_the_trigram_embedder_are_reported_from_the_marker() {
        let p = Project::new();
        p.embeddings(10);
        marker(&p, "trigram");
        assert_eq!(
            embedder_modes(p.root(), None),
            vec![DegradedMode::EmbeddingsBuiltWithTrigram]
        );
    }

    #[test]
    fn an_absent_marker_or_a_model2vec_one_reports_nothing() {
        let p = Project::new();
        p.embeddings(10);
        assert_eq!(embedder_modes(p.root(), None), vec![]);
        assert_eq!(embedder_modes(p.root(), Some("trigram")), vec![]);
        marker(&p, "model2vec");
        assert_eq!(embedder_modes(p.root(), None), vec![]);
        assert_eq!(embedder_modes(p.root(), Some("model2vec")), vec![]);
    }

    #[test]
    fn a_query_embedder_that_differs_from_the_one_that_built_the_index_is_reported() {
        let p = Project::new();
        p.embeddings(10);
        marker(&p, "model2vec");
        assert_eq!(
            embedder_modes(p.root(), Some("trigram")),
            vec![DegradedMode::EmbedderMismatch {
                built: "model2vec".to_string(),
                query: "trigram".to_string(),
            }]
        );
        marker(&p, "trigram");
        assert_eq!(
            embedder_modes(p.root(), Some("model2vec")),
            vec![
                DegradedMode::EmbeddingsBuiltWithTrigram,
                DegradedMode::EmbedderMismatch {
                    built: "trigram".to_string(),
                    query: "model2vec".to_string(),
                }
            ]
        );
    }

    #[test]
    fn embeddings_from_two_embedders_are_reported_as_mixed() {
        let p = Project::new();
        p.embeddings(10);
        marker(&p, "mixed");
        assert_eq!(
            embedder_modes(p.root(), Some("model2vec")),
            vec![DegradedMode::EmbeddingsMixed]
        );
    }

    // --- what a live process reports about itself ---

    #[test]
    fn a_live_mode_is_set_replaced_and_cleared_by_key_per_root() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        live::set(
            a.path(),
            DegradedMode::UnwatchedDirectories {
                failed: 2,
                first: "x: denied".to_string(),
            },
        );
        live::set(
            a.path(),
            DegradedMode::UnwatchedDirectories {
                failed: 5,
                first: "y: denied".to_string(),
            },
        );
        let for_a = live::for_root(a.path());
        assert_eq!(for_a.len(), 1, "the same key replaces: {for_a:?}");
        assert!(for_a[0].message.contains('5'), "{}", for_a[0].message);
        assert_eq!(live::for_root(b.path()), vec![]);

        live::clear(a.path(), UNWATCHED_DIRECTORIES);
        assert_eq!(live::for_root(a.path()), vec![]);
    }

    #[test]
    fn the_embeddings_generation_check_sets_and_clears_the_live_mode() {
        let p = Project::new();
        p.embeddings(10);
        let sidecar = p.root().join(".infigraph/embeddings.bin");
        crate::embed::write_generation_marker(&sidecar, 3).unwrap();

        live::note_embeddings_generation(p.root(), 7);
        let notices = live::for_root(p.root());
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].key, EMBEDDINGS_STALE);
        assert!(notices[0].message.contains('3') && notices[0].message.contains('7'));

        crate::embed::write_generation_marker(&sidecar, 7).unwrap();
        live::note_embeddings_generation(p.root(), 7);
        assert_eq!(live::for_root(p.root()), vec![]);

        // No marker: cannot judge, so it is not reported.
        std::fs::remove_file(p.root().join(".infigraph/embeddings.bin.generation")).unwrap();
        live::note_embeddings_generation(p.root(), 9);
        assert_eq!(live::for_root(p.root()), vec![]);
    }

    /// The daemon's answer for a key replaces what this process derived for
    /// it: the daemon compares generations exactly, where the local rule for
    /// staleness is a file-time heuristic.
    #[test]
    fn the_daemons_notice_for_a_key_replaces_the_local_one() {
        let local = vec![
            DegradedMode::EmbeddingsStale { minutes: 90 }.notice(),
            DegradedMode::HnswMissing.notice(),
        ];
        let from_daemon = vec![
            DegradedMode::EmbeddingsBehindGraph {
                recorded: 3,
                current: 7,
            }
            .notice(),
            DegradedMode::DocReadsUnavailable {
                reason: "will not open".to_string(),
            }
            .notice(),
        ];
        let merged = merge(local, from_daemon.clone());
        let keys: Vec<&str> = merged.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![EMBEDDINGS_STALE, "hnsw-missing", "doc-reads-unavailable"]
        );
        assert_eq!(merged[0], from_daemon[0], "the daemon's wording wins");
    }

    /// When a daemon answers and does not report staleness, the local
    /// file-time guess is dropped: the daemon knows the generations.
    #[test]
    fn a_daemon_that_answers_overrules_the_local_staleness_guess() {
        let local = vec![
            DegradedMode::EmbeddingsStale { minutes: 90 }.notice(),
            DegradedMode::HnswMissing.notice(),
        ];
        let keys: Vec<String> = merge_with_daemon(local.clone(), Some(vec![]))
            .into_iter()
            .map(|n| n.key)
            .collect();
        assert_eq!(keys, vec!["hnsw-missing"]);
        // No daemon: the local rule stands.
        assert_eq!(merge_with_daemon(local.clone(), None), local);
    }

    /// A notice from a newer build, with a key this build has no variant for,
    /// still deserializes: it is strings, not the enum.
    #[test]
    fn a_notice_with_an_unknown_key_still_reads() {
        let json = r#"{"key":"some-future-mode","message":"m","remedy":"r"}"#;
        let notice: Notice = serde_json::from_str(json).unwrap();
        assert_eq!(notice.key, "some-future-mode");
    }
}
