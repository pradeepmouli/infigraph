use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use rayon::prelude::*;

use infigraph_core::embed::{
    doc_embedder, invalidate_embeddings_cache, load_embeddings, record_unchanged_generation,
    save_embeddings, sync_hnsw_index_delta, EmbStamp, HnswDelta, HnswSync,
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
    if !emb_path.exists() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    record_unchanged_generation(&emb_path, &tg_dir.join(DOCS_HNSW_FILE), now);
}

pub fn update_doc_embeddings(
    store: &dyn DocBackend,
    root: &Path,
    new_chunks: &[&Chunk],
    changed_files: &[&str],
) -> Result<usize> {
    update_doc_embeddings_synced(store, root, new_chunks, changed_files).map(|(count, _)| count)
}

/// [`update_doc_embeddings`], also saying what it did to the HNSW index
/// (`None`: there is none to keep, below the size that earns one).
pub(crate) fn update_doc_embeddings_synced(
    store: &dyn DocBackend,
    root: &Path,
    new_chunks: &[&Chunk],
    changed_files: &[&str],
) -> Result<(usize, Option<HnswSync>)> {
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

    // Embed new chunks with file path context for better semantic matching
    let to_embed: Vec<(&str, String)> = new_chunks
        .iter()
        .map(|c| {
            let file_context = doc_path_context(&c.doc_file);
            let text = match (&file_context, &c.heading) {
                (Some(ctx), Some(h)) => format!("{} > {}: {}", ctx, h, c.text),
                (Some(ctx), None) => format!("{}: {}", ctx, c.text),
                (None, Some(h)) => format!("{}: {}", h, c.text),
                (None, None) => c.text.clone(),
            };
            (c.id.as_str(), text)
        })
        .collect();

    if !to_embed.is_empty() {
        let embedder = doc_embedder();
        const BATCH: usize = 256;
        let results: Vec<Vec<(String, Vec<f32>)>> = to_embed
            .par_chunks(BATCH)
            .map(|chunk| {
                let emb = Arc::clone(&embedder);
                let texts: Vec<&str> = chunk.iter().map(|(_, t)| t.as_str()).collect();
                let vecs = emb.embed_batch(&texts).unwrap_or_default();
                chunk
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (id, _))| vecs.get(i).map(|v| (id.to_string(), v.clone())))
                    .collect()
            })
            .collect();
        for batch in results {
            for (id, v) in batch {
                upserted.push((id.clone(), v.clone()));
                existing.insert(id, v);
            }
        }
    }

    // Also keep existing chunks that are still in the store
    let all_store_chunks = store.get_all_chunks().unwrap_or_default();
    let valid_ids: std::collections::HashSet<String> =
        all_store_chunks.into_iter().map(|(id, _)| id).collect();
    existing.retain(|id, _| {
        let keep = valid_ids.contains(id);
        if !keep {
            removed_ids.push(id.clone());
        }
        keep
    });

    let embeddings: Vec<(String, Vec<f32>)> = existing.into_iter().collect();
    let count = embeddings.len();
    save_embeddings(&emb_path, &embeddings)?;

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
    Ok((count, synced))
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
        const BATCH: usize = 256;
        let results: Vec<Vec<(String, Vec<f32>)>> = to_embed
            .par_chunks(BATCH)
            .map(|chunk| {
                let emb = Arc::clone(&embedder);
                let texts: Vec<&str> = chunk.iter().map(|(_, t)| *t).collect();
                let vecs = emb.embed_batch(&texts).unwrap_or_default();
                chunk
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (id, _))| vecs.get(i).map(|v| (id.to_string(), v.clone())))
                    .collect()
            })
            .collect();
        all_embeddings = results.into_iter().flatten().collect();
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
            count,
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
