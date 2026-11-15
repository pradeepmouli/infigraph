use std::path::Path;

use std::sync::Arc;

use anyhow::Result;
use infigraph_core::embed::{
    doc_embedder, embed_in_batches, invalidate_embeddings_cache, load_embeddings,
    read_unembedded_count, record_unchanged_generation, save_embeddings, sync_hnsw_index_delta,
    write_unembedded_count, EmbStamp, EmbedProvider, HnswDelta, HnswSync,
};

use crate::backend::DocBackend;
use crate::chunk::Chunk;

/// The document vectors' HNSW index, beside `docs_embeddings.bin`.
const DOCS_HNSW_FILE: &str = "docs_hnsw_index.usearch";

/// A document index run reached its end without an error: the store holds what
/// it was just told to, so `docs_embeddings.bin` is confirmed against it, even
/// when nothing needed embedding and the file kept its bytes (a run that only
/// moves links, acknowledgements or scan rows still moves `docs.kuzu`, and
/// without this the sidecar read as older than its store for good). Moves the
/// generation marker through the helper the code side uses for the same case,
/// which also re-stamps the HNSW index that reflects the file. Documents have
/// no generation counter, so the marker's value is the time of the run; the
/// only reader (`doctor::sidecar_lag`) uses the file's time. A project with no
/// `docs_embeddings.bin` gets none.
pub fn confirm_doc_embeddings(root: &Path) {
    let tg_dir = root.join(".infigraph");
    let emb_path = tg_dir.join("docs_embeddings.bin");
    // A file that lacks vectors for chunks the store holds is not "up to date
    // with it", whatever the run that is ending changed: the marker moves only
    // when nothing is missing (see `read_unembedded_count`).
    if !emb_path.exists() || read_unembedded_count(&emb_path) > 0 {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    record_unchanged_generation(&emb_path, &tg_dir.join(DOCS_HNSW_FILE), now);
}

/// What an update of `docs_embeddings.bin` left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocEmbedOutcome {
    /// Vectors in the file.
    pub stored: usize,
    /// Chunks the store holds that have no vector in it (the embedder failed or
    /// returned too few, now or on an earlier run). Also recorded beside the
    /// file (`write_unembedded_count`), where a process that did not run the
    /// update reads it.
    pub not_embedded: usize,
}

/// The text a chunk is embedded as: its file's path context and heading in front
/// of its text, so a query that names the topic or the place matches it.
fn chunk_embed_text(doc_file: &str, heading: Option<&str>, text: &str) -> String {
    match (doc_path_context(doc_file), heading) {
        (Some(ctx), Some(h)) => format!("{ctx} > {h}: {text}"),
        (Some(ctx), None) => format!("{ctx}: {text}"),
        (None, Some(h)) => format!("{h}: {text}"),
        (None, None) => text.to_string(),
    }
}

/// A docs run that had nothing new to embed: if an earlier run left chunks
/// without a vector (`read_unembedded_count`), try them once more, so the
/// shortfall closes without waiting for their files to change. `true` when it
/// ran an update (the file was rewritten, so the caller does not confirm it).
/// A failing embedder costs one empty batch call and one log line per run.
pub fn retry_unembedded(store: &dyn DocBackend, root: &Path) -> bool {
    retry_unembedded_with(store, root, &doc_embedder)
}

/// [`retry_unembedded`] with the embedder it runs, for a test that has to fail one.
pub(crate) fn retry_unembedded_with(
    store: &dyn DocBackend,
    root: &Path,
    make_embedder: &dyn Fn() -> Arc<dyn EmbedProvider>,
) -> bool {
    let emb_path = root.join(".infigraph").join("docs_embeddings.bin");
    if read_unembedded_count(&emb_path) == 0 {
        return false;
    }
    update_doc_embeddings_with(store, root, &[], &[], make_embedder).is_ok()
}

pub fn update_doc_embeddings(
    store: &dyn DocBackend,
    root: &Path,
    new_chunks: &[&Chunk],
    changed_files: &[&str],
) -> Result<DocEmbedOutcome> {
    update_doc_embeddings_synced(store, root, new_chunks, changed_files).map(|(out, _)| out)
}

/// [`update_doc_embeddings`], also saying what it did to the HNSW index
/// (`None`: there is none to keep, below the size that earns one).
pub(crate) fn update_doc_embeddings_synced(
    store: &dyn DocBackend,
    root: &Path,
    new_chunks: &[&Chunk],
    changed_files: &[&str],
) -> Result<(DocEmbedOutcome, Option<HnswSync>)> {
    update_doc_embeddings_with(store, root, new_chunks, changed_files, &doc_embedder)
}

/// [`update_doc_embeddings_synced`] with the embedder it runs (asked for only
/// when something needs embedding, as the singleton loads a model).
pub(crate) fn update_doc_embeddings_with(
    store: &dyn DocBackend,
    root: &Path,
    new_chunks: &[&Chunk],
    changed_files: &[&str],
    make_embedder: &dyn Fn() -> Arc<dyn EmbedProvider>,
) -> Result<(DocEmbedOutcome, Option<HnswSync>)> {
    let tg_dir = root.join(".infigraph");
    let emb_path = tg_dir.join("docs_embeddings.bin");
    // Which file the HNSW index on disk reflects, taken before this run writes
    // a new one: a patch applies only to an index that saw the previous file.
    let prior = EmbStamp::of(&emb_path);

    let mut existing: std::collections::HashMap<String, Vec<f32>> = load_embeddings(&emb_path)
        .unwrap_or_default()
        .into_iter()
        .collect();

    let changed_set: std::collections::HashSet<&str> = changed_files.iter().copied().collect();

    // What this run took out of, and put into, the vectors, for the HNSW patch.
    let mut removed_ids: Vec<String> = Vec::new();
    let mut upserted: Vec<(String, Vec<f32>)> = Vec::new();

    // Remove embeddings for chunks belonging to changed files
    if !changed_set.is_empty() {
        existing.retain(|id, _| {
            let file = id.split("::chunk_").next().unwrap_or("");
            let keep = !changed_set.contains(file);
            if !keep {
                removed_ids.push(id.clone());
            }
            keep
        });
    }

    let all_store_chunks = store.get_all_chunks().unwrap_or_default();
    let valid_ids: std::collections::HashSet<String> =
        all_store_chunks.into_iter().map(|(id, _)| id).collect();

    // Embed new chunks with file path context for better semantic matching.
    let mut to_embed: Vec<(String, String)> = new_chunks
        .iter()
        .map(|c| {
            (
                c.id.clone(),
                chunk_embed_text(&c.doc_file, c.heading.as_deref(), &c.text),
            )
        })
        .collect();
    // And one more attempt, per run, at every chunk the store holds that still
    // has no vector: an embedder that failed on an earlier run left them out,
    // and nothing else would put them back until their file changed.
    let new_ids: std::collections::HashSet<&str> =
        new_chunks.iter().map(|c| c.id.as_str()).collect();
    let retry_ids: Vec<&str> = valid_ids
        .iter()
        .map(String::as_str)
        .filter(|id| !existing.contains_key(*id) && !new_ids.contains(id))
        .collect();
    if !retry_ids.is_empty() {
        for d in store.get_chunk_details(&retry_ids).unwrap_or_default() {
            to_embed.push((
                d.id.clone(),
                chunk_embed_text(&d.doc_file, d.heading.as_deref(), &d.text),
            ));
        }
    }

    if !to_embed.is_empty() {
        let embedder = make_embedder();
        let texts: Vec<&str> = to_embed.iter().map(|(_, t)| t.as_str()).collect();
        let batched = embed_in_batches(&*embedder, &texts);
        let failure = batched.failure.clone();
        for (i, v) in batched.vectors {
            let id = to_embed[i].0.clone();
            upserted.push((id.clone(), v.clone()));
            existing.insert(id, v);
        }
        if let Some(reason) = failure {
            // One line for the run, whatever the number of chunks.
            let left = valid_ids
                .iter()
                .filter(|id| !existing.contains_key(*id))
                .count();
            eprintln!("[docs-embeddings] {left} chunk(s) not embedded: {reason}");
        }
    }

    // Also keep existing chunks that are still in the store
    existing.retain(|id, _| {
        let keep = valid_ids.contains(id);
        if !keep {
            removed_ids.push(id.clone());
        }
        keep
    });
    let not_embedded = valid_ids
        .iter()
        .filter(|id| !existing.contains_key(*id))
        .count();

    let embeddings: Vec<(String, Vec<f32>)> = existing.into_iter().collect();
    let count = embeddings.len();
    save_embeddings(&emb_path, &embeddings)?;
    // After the file, so a marker never claims a count for bytes that were not
    // written; best-effort like the others (a failed write leaves the earlier
    // count, which `confirm_doc_embeddings` still honours).
    let _ = write_unembedded_count(&emb_path, not_embedded);

    // Build HNSW if above threshold or existing index
    const HNSW_THRESHOLD: usize = 200_000;
    let hnsw_path = tg_dir.join(DOCS_HNSW_FILE);
    let synced = if count >= HNSW_THRESHOLD || hnsw_path.exists() {
        // One edited document patches the index in place, as the code side
        // does; a large change, or an index that missed a write, rebuilds it.
        let delta = HnswDelta {
            upserted: &upserted,
            removed: &removed_ids,
        };
        Some(sync_hnsw_index_delta(
            &embeddings,
            &delta,
            prior,
            &hnsw_path,
            &emb_path,
            1,
        ))
    } else {
        None
    };

    invalidate_embeddings_cache();
    Ok((
        DocEmbedOutcome {
            stored: count,
            not_embedded,
        },
        synced,
    ))
}

/// Store doc chunk embeddings in Postgres pgvector (remote mode).
/// Embeds new/changed chunks plus any store chunks missing from pgvector.
#[cfg(feature = "remote")]
pub fn update_doc_embeddings_remote(
    store: &dyn DocBackend,
    pg: &infigraph_core::meta::PostgresMetaStore,
    _new_chunks: &[&Chunk],
    changed_files: &[&str],
) -> Result<usize> {
    let existing_ids: std::collections::HashSet<String> = pg
        .all_embeddings("doc_chunk")
        .unwrap_or_default()
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    let changed_set: std::collections::HashSet<&str> = changed_files.iter().copied().collect();

    // All chunks currently in the store (id, text)
    let all_store_chunks = store.get_all_chunks().unwrap_or_default();

    // Embed: (1) chunks from changed files, (2) chunks missing from pgvector
    let to_embed: Vec<(&str, &str)> = all_store_chunks
        .iter()
        .filter(|(id, _)| {
            let doc_file = id.split("::chunk_").next().unwrap_or("");
            if changed_set.contains(doc_file) {
                return true;
            }
            !existing_ids.contains(id.as_str())
        })
        .map(|(id, text)| (id.as_str(), text.as_str()))
        .collect();

    let mut all_embeddings: Vec<(String, Vec<f32>)> = Vec::new();

    if !to_embed.is_empty() {
        let embedder = doc_embedder();
        let texts: Vec<&str> = to_embed.iter().map(|(_, t)| *t).collect();
        let batched = embed_in_batches(&*embedder, &texts);
        // Asked for again by the next update (pgvector still lacks them), so
        // reported here and not recorded.
        if let Some(reason) = &batched.failure {
            eprintln!(
                "[docs-embeddings] {} chunk(s) not embedded: {reason}",
                batched.shortfall(to_embed.len())
            );
        }
        all_embeddings = batched
            .vectors
            .into_iter()
            .map(|(i, v)| (to_embed[i].0.to_string(), v))
            .collect();
    }

    let count = all_embeddings.len();
    if !all_embeddings.is_empty() {
        pg.upsert_embeddings_bulk(&all_embeddings, "doc_chunk")?;
    }

    // Clean up orphan doc_chunk embeddings
    let valid_ids: std::collections::HashSet<&str> =
        all_store_chunks.iter().map(|(id, _)| id.as_str()).collect();
    let orphans: Vec<String> = existing_ids
        .iter()
        .filter(|id| !valid_ids.contains(id.as_str()))
        .cloned()
        .collect();
    if !orphans.is_empty() {
        pg.delete_embeddings(&orphans)?;
    }

    Ok(count + valid_ids.len().saturating_sub(count))
}

fn doc_path_context(file: &str) -> Option<String> {
    let parts: Vec<&str> = file.split('/').collect();
    if parts.len() <= 1 {
        return None;
    }
    let stem = parts
        .last()?
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(parts.last()?);
    let name = stem.replace(['_', '-'], " ");
    let dirs: Vec<&str> = parts[..parts.len() - 1]
        .iter()
        .filter(|p| {
            let lower = p.to_lowercase();
            !matches!(
                lower.as_str(),
                "src" | "doc" | "docs" | "documentation" | "resources"
            )
        })
        .copied()
        .collect();
    if dirs.is_empty() {
        Some(name)
    } else {
        let dir_path = dirs.join("/");
        Some(format!("{}/{}", dir_path, name))
    }
}

#[cfg(test)]
mod hnsw_patch_tests {
    //! One edited document patches the document HNSW index in place, as the
    //! code side's does, instead of rebuilding every vector.
    use super::*;
    use crate::extract::{DocFormat, ExtractedDoc};
    use crate::DocIndex;
    use infigraph_core::embed::build_hnsw_index;

    fn doc(file: &str) -> ExtractedDoc {
        ExtractedDoc {
            file: file.to_string(),
            title: None,
            content_hash: format!("hash-{file}"),
            format: DocFormat::Markdown,
            text: "body".to_string(),
            page_count: None,
        }
    }

    fn chunk(file: &str, index: usize) -> Chunk {
        Chunk {
            id: format!("{file}::chunk_{index}"),
            doc_file: file.to_string(),
            content_hash: format!("hash-{file}"),
            index,
            heading: Some(format!("Heading {index}")),
            text: format!("{file} section {index}: some ordinary prose about the system"),
            start_offset: 0,
            end_offset: 60,
            page: None,
        }
    }

    /// A store of 60 two-chunk documents with embeddings and an HNSW index.
    struct Fixture {
        _home: tempfile::TempDir,
        _tmp: tempfile::TempDir,
        root: std::path::PathBuf,
        idx: DocIndex,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();
        let mut idx = DocIndex::open(&root).unwrap();
        idx.init().unwrap();
        let docs: Vec<ExtractedDoc> = (0..60).map(|d| doc(&format!("docs/d{d}.md"))).collect();
        let chunks: Vec<Chunk> = (0..60)
            .flat_map(|d| (0..2).map(move |c| chunk(&format!("docs/d{d}.md"), c)))
            .collect();
        let store = idx.store().unwrap();
        store
            .upsert_docs(
                &docs.iter().collect::<Vec<_>>(),
                &chunks.iter().collect::<Vec<_>>(),
            )
            .unwrap();
        let files: Vec<String> = docs.iter().map(|d| d.file.clone()).collect();
        let file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
        update_doc_embeddings(store, &root, &chunks.iter().collect::<Vec<_>>(), &file_refs)
            .unwrap();
        let emb = root.join(".infigraph").join("docs_embeddings.bin");
        build_hnsw_index(
            &load_embeddings(&emb).unwrap(),
            &root.join(".infigraph").join("docs_hnsw_index.usearch"),
            &emb,
        )
        .unwrap();
        Fixture {
            _home: home,
            _tmp: tmp,
            root,
            idx,
        }
    }

    /// Replaces `docs/d0.md`'s chunks with three, in the store and as the
    /// chunks the update is told about.
    fn edit_d0(f: &Fixture) -> Vec<Chunk> {
        let new: Vec<Chunk> = (0..3).map(|c| chunk("docs/d0.md", c)).collect();
        let store = f.idx.store().unwrap();
        store.delete_docs_by_ids(&["docs/d0.md"]).unwrap();
        store
            .upsert_docs(&[&doc("docs/d0.md")], &new.iter().collect::<Vec<_>>())
            .unwrap();
        new
    }

    #[test]
    fn one_edited_document_patches_the_index_in_place() {
        let f = fixture();
        let new = edit_d0(&f);

        let (count, synced) = update_doc_embeddings_synced(
            f.idx.store().unwrap(),
            &f.root,
            &new.iter().collect::<Vec<_>>(),
            &["docs/d0.md"],
        )
        .unwrap();

        assert_eq!(
            count.stored,
            60 * 2 + 1,
            "119 untouched vectors plus the three new"
        );
        assert_eq!(synced, Some(HnswSync::Updated));
    }

    #[test]
    fn an_index_that_missed_a_write_is_rebuilt_not_patched() {
        let f = fixture();
        // Someone rewrote embeddings.bin without the index seeing it.
        let emb = f.root.join(".infigraph").join("docs_embeddings.bin");
        let vectors = load_embeddings(&emb).unwrap();
        save_embeddings(&emb, &vectors).unwrap();
        let new = edit_d0(&f);

        let (_, synced) = update_doc_embeddings_synced(
            f.idx.store().unwrap(),
            &f.root,
            &new.iter().collect::<Vec<_>>(),
            &["docs/d0.md"],
        )
        .unwrap();

        assert_eq!(synced, Some(HnswSync::Rebuilt));
    }

    #[test]
    fn a_store_too_small_for_an_index_keeps_none() {
        let f = fixture();
        std::fs::remove_file(f.root.join(".infigraph").join("docs_hnsw_index.usearch")).unwrap();
        let new = edit_d0(&f);

        let (_, synced) = update_doc_embeddings_synced(
            f.idx.store().unwrap(),
            &f.root,
            &new.iter().collect::<Vec<_>>(),
            &["docs/d0.md"],
        )
        .unwrap();

        assert_eq!(synced, None);
    }

    // ---- chunks the embedder could not embed ----

    use infigraph_core::embed::{read_unembedded_count, TrigramEmbedder};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Embeds like the trigram embedder, or fails every batch while `fail` is set.
    struct Flaky {
        fail: Arc<AtomicBool>,
        inner: TrigramEmbedder,
    }

    impl EmbedProvider for Flaky {
        fn dimension(&self) -> usize {
            self.inner.dimension()
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            if self.fail.load(Ordering::SeqCst) {
                anyhow::bail!("the model is not loaded");
            }
            self.inner.embed_batch(texts)
        }
        fn name(&self) -> &'static str {
            "flaky"
        }
    }

    fn flaky(fail: &Arc<AtomicBool>) -> impl Fn() -> Arc<dyn EmbedProvider> {
        let fail = Arc::clone(fail);
        move || {
            Arc::new(Flaky {
                fail: Arc::clone(&fail),
                inner: TrigramEmbedder::default(),
            })
        }
    }

    /// The fixture's store plus one new two-chunk document that is in the store
    /// but not yet embedded.
    fn with_an_unembedded_document(f: &Fixture) -> Vec<Chunk> {
        let file = "docs/new.md";
        let chunks = vec![chunk(file, 0), chunk(file, 1)];
        f.idx
            .store()
            .unwrap()
            .upsert_docs(&[&doc(file)], &chunks.iter().collect::<Vec<_>>())
            .unwrap();
        chunks
    }

    fn sidecar(f: &Fixture) -> std::path::PathBuf {
        f.root.join(".infigraph").join("docs_embeddings.bin")
    }

    #[test]
    fn a_failed_batch_is_counted_recorded_and_reported_not_swallowed() {
        let f = fixture();
        let chunks = with_an_unembedded_document(&f);
        let fail = Arc::new(AtomicBool::new(true));
        let (out, _) = update_doc_embeddings_with(
            f.idx.store().unwrap(),
            &f.root,
            &chunks.iter().collect::<Vec<_>>(),
            &["docs/new.md"],
            &flaky(&fail),
        )
        .unwrap();
        assert_eq!(out.not_embedded, 2);
        assert_eq!(out.stored, 60 * 2, "the 120 earlier vectors are kept");
        // On disk, so a process that did not run the update sees it.
        assert_eq!(read_unembedded_count(&sidecar(&f)), 2);
        let modes = infigraph_core::degraded::derived_from_disk(&f.root);
        assert!(
            modes.contains(
                &infigraph_core::degraded::DegradedMode::DocEmbeddingsPartial { chunks: 2 }
            ),
            "{modes:?}"
        );
    }

    /// The marker that says the file is up to date with its store must not
    /// move while chunks have no vector; it moves once they have one.
    #[test]
    fn the_file_is_not_confirmed_while_chunks_lack_vectors() {
        let f = fixture();
        let chunks = with_an_unembedded_document(&f);
        let fail = Arc::new(AtomicBool::new(true));
        update_doc_embeddings_with(
            f.idx.store().unwrap(),
            &f.root,
            &chunks.iter().collect::<Vec<_>>(),
            &["docs/new.md"],
            &flaky(&fail),
        )
        .unwrap();
        let bin = sidecar(&f);
        let marker = f.root.join(".infigraph/docs_embeddings.bin.generation");
        let _ = std::fs::remove_file(&marker);

        confirm_doc_embeddings(&f.root);
        assert!(!marker.exists(), "confirmed a file that lacks vectors");

        fail.store(false, Ordering::SeqCst);
        assert!(retry_unembedded_with(
            f.idx.store().unwrap(),
            &f.root,
            &flaky(&fail)
        ));
        assert_eq!(read_unembedded_count(&bin), 0);
        confirm_doc_embeddings(&f.root);
        assert!(marker.exists(), "a complete file is confirmed again");
    }

    /// The next run that can embed them does, and the marker, the mode and the
    /// file's own count all agree afterwards.
    #[test]
    fn a_later_run_embeds_what_an_earlier_one_could_not() {
        let f = fixture();
        let chunks = with_an_unembedded_document(&f);
        let fail = Arc::new(AtomicBool::new(true));
        update_doc_embeddings_with(
            f.idx.store().unwrap(),
            &f.root,
            &chunks.iter().collect::<Vec<_>>(),
            &["docs/new.md"],
            &flaky(&fail),
        )
        .unwrap();
        fail.store(false, Ordering::SeqCst);
        // Nothing new and nothing changed: only the retry brings them in.
        let (out, _) =
            update_doc_embeddings_with(f.idx.store().unwrap(), &f.root, &[], &[], &flaky(&fail))
                .unwrap();
        assert_eq!(out.not_embedded, 0);
        assert_eq!(out.stored, 60 * 2 + 2);
        assert_eq!(read_unembedded_count(&sidecar(&f)), 0);
        assert!(!f
            .root
            .join(".infigraph/docs_embeddings.bin.unembedded")
            .exists());
        assert!(infigraph_core::degraded::derived_from_disk(&f.root)
            .iter()
            .all(|m| m.key() != "doc-embeddings-partial"));
    }

    /// An embedder that fails every run costs one attempt per run: the count
    /// stays what it was and the file keeps every vector it has.
    #[test]
    fn a_permanently_failing_embedder_is_one_attempt_per_run() {
        let f = fixture();
        let chunks = with_an_unembedded_document(&f);
        let fail = Arc::new(AtomicBool::new(true));
        update_doc_embeddings_with(
            f.idx.store().unwrap(),
            &f.root,
            &chunks.iter().collect::<Vec<_>>(),
            &["docs/new.md"],
            &flaky(&fail),
        )
        .unwrap();
        for _ in 0..3 {
            assert!(retry_unembedded_with(
                f.idx.store().unwrap(),
                &f.root,
                &flaky(&fail)
            ));
            assert_eq!(read_unembedded_count(&sidecar(&f)), 2);
        }
        assert_eq!(load_embeddings(&sidecar(&f)).unwrap().len(), 60 * 2);
    }

    #[test]
    fn a_run_with_nothing_missing_does_not_retry() {
        let f = fixture();
        let fail = Arc::new(AtomicBool::new(true));
        assert!(!retry_unembedded_with(
            f.idx.store().unwrap(),
            &f.root,
            &flaky(&fail)
        ));
    }
}

#[cfg(test)]
mod confirm_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn age(path: &Path, by: Duration) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - by)
            .unwrap();
    }

    #[test]
    fn a_confirmed_sidecar_keeps_its_bytes_and_gets_a_new_marker() {
        let dir = tempfile::tempdir().unwrap();
        let ig = dir.path().join(".infigraph");
        std::fs::create_dir_all(&ig).unwrap();
        let bin = ig.join("docs_embeddings.bin");
        std::fs::write(&bin, b"vectors").unwrap();
        age(&bin, Duration::from_secs(3 * 3600));

        confirm_doc_embeddings(dir.path());

        assert_eq!(std::fs::read(&bin).unwrap(), b"vectors", "bytes untouched");
        assert!(infigraph_core::embed::read_generation_marker(&bin).is_some_and(|g| g > 0));
        let marker = ig.join("docs_embeddings.bin.generation");
        let bin_age = bin.metadata().unwrap().modified().unwrap();
        assert!(marker.metadata().unwrap().modified().unwrap() > bin_age);
    }

    #[test]
    fn a_project_with_no_docs_embeddings_gets_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
        confirm_doc_embeddings(dir.path());
        assert!(!dir
            .path()
            .join(".infigraph/docs_embeddings.bin.generation")
            .exists());
        assert!(!dir.path().join(".infigraph/docs_embeddings.bin").exists());
    }
}
