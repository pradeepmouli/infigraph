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
    /// `embeddings.bin` is older than the graph it was built from.
    EmbeddingsStale { minutes: u64 },
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
            DegradedMode::EmbeddingsStale { .. } => "embeddings-stale",
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
            | DegradedMode::HnswMissing => "run `infigraph index` to rebuild them",
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
pub fn gather(root: &Path) -> Vec<Notice> {
    let mut modes = Vec::new();
    if crate::embed::trigram_fallback_active() {
        modes.push(DegradedMode::TrigramEmbedder);
    }
    modes.extend(embedder_modes(root, crate::embed::process_embedder()));
    modes.extend(derived_from_disk(root));
    modes.iter().map(DegradedMode::notice).collect()
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
    if crate::docs_switch::docs_indexed(root) && !ig.join("docs_embeddings.bin").exists() {
        modes.push(DegradedMode::DocEmbeddingsMissing);
    }
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

    #[test]
    fn indexed_documents_without_embeddings_are_reported_only_when_docs_are_on() {
        let p = Project::new();
        p.embeddings(10);
        // A leftover store in a project that has not opted in is not ours to judge.
        p.write("docs.kuzu", b"docs");
        assert_eq!(derived_from_disk(p.root()), vec![]);

        p.enable_docs();
        assert_eq!(
            derived_from_disk(p.root()),
            vec![DegradedMode::DocEmbeddingsMissing]
        );

        p.write("docs_embeddings.bin", &1u32.to_le_bytes());
        assert_eq!(derived_from_disk(p.root()), vec![]);
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

    /// A notice from a newer build, with a key this build has no variant for,
    /// still deserializes: it is strings, not the enum.
    #[test]
    fn a_notice_with_an_unknown_key_still_reads() {
        let json = r#"{"key":"some-future-mode","message":"m","remedy":"r"}"#;
        let notice: Notice = serde_json::from_str(json).unwrap();
        assert_eq!(notice.key, "some-future-mode");
    }
}
