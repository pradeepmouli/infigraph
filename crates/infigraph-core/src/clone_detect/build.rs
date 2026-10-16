//! Builds `clone-bodies.bin` as part of every embeddings update.
//!
//! The body text comes from the source file, read once into one buffer. That
//! buffer is hashed and compared with the `content_hash` the graph recorded
//! when it wrote the file's symbols, and the symbols' line spans are applied
//! to the same buffer. When the hashes agree the bytes are the ones the
//! extraction saw, so the spans are right by construction; when they do not
//! (the file was edited after it was indexed) the file's symbols get no entry
//! this round and the re-index the edit triggers fills them. A sidecar never
//! holds text taken from a file the graph was not written from.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use super::bodies::{self, BodyEntry, BODIES_FILE};
use super::normalize::{normalize, normalized_text};
use super::sketch::sketch;
use crate::degraded::{live, DegradedMode, CLONE_BODIES_PARTIAL};
use crate::embed::{fnv1a64, EmbedProvider};
use crate::graph::GraphBackend;

/// Cap on the text one body hands the embedder: a model embeds a bounded
/// window anyway, and the sketch covers the whole body.
const MAX_EMBED_CHARS: usize = 4000;

const BATCH: usize = 256;

const SYMBOLS: &str = "MATCH (s:Symbol) WHERE s.kind IN ['Function', 'Method'] \
     RETURN s.id, s.file, s.start_line, s.end_line, s.language, s.signature_hash";

/// What one update did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BuildReport {
    /// Function and method symbols in the graph.
    pub symbols: usize,
    /// Of those, symbols the sidecar describes with a body embedding.
    pub embedded: usize,
    /// Body texts sent to the embedder this update.
    pub embed_requests: usize,
    /// Files skipped because they changed after they were indexed.
    pub files_changed_since_index: usize,
}

struct Wanted {
    id: String,
    file: String,
    start_line: usize,
    end_line: usize,
    language: String,
    signature: u64,
}

/// A body read and normalized, not yet embedded.
struct Prepared {
    id: String,
    signature: u64,
    token_count: u32,
    sketch: super::sketch::Sketch,
    text: String,
    input_hash: u64,
}

fn parse(rows: Vec<Vec<String>>) -> Vec<Wanted> {
    rows.into_iter()
        .filter_map(|row| {
            let [id, file, start, end, language, signature]: [String; 6] = row.try_into().ok()?;
            Some(Wanted {
                id,
                file,
                start_line: start.parse().ok()?,
                end_line: end.parse().ok()?,
                language,
                signature: bodies::signature_of(&signature),
            })
        })
        .collect()
}

/// Reads `file` once and prepares `symbols` from that buffer, or returns
/// `None` when the buffer is not the file the graph was written from.
fn prepare_file(
    root: &Path,
    file: &str,
    recorded_hash: Option<&String>,
    symbols: &[&Wanted],
) -> Option<Vec<Prepared>> {
    let bytes = std::fs::read(root.join(file)).ok()?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if recorded_hash != Some(&actual) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let prepared = symbols
        .iter()
        .filter_map(|w| {
            let from = w.start_line.saturating_sub(1).min(lines.len());
            let to = w.end_line.max(w.start_line).min(lines.len());
            let body = lines[from..to].join("\n");
            let tokens = normalize(&body, &w.language);
            let sketch = sketch(&tokens)?;
            let mut embed_text = normalized_text(&body, &w.language);
            embed_text.truncate(embed_text.floor_char_boundary(MAX_EMBED_CHARS));
            Some(Prepared {
                id: w.id.clone(),
                signature: w.signature,
                token_count: tokens.len() as u32,
                sketch,
                input_hash: fnv1a64(embed_text.as_bytes()),
                text: embed_text,
            })
        })
        .collect();
    Some(prepared)
}

/// Brings `clone-bodies.bin` up to date with the graph.
///
/// A symbol is in scope when its file is in `changed_files` (all of them when
/// that is empty), when the sidecar has no entry for it, or when its entry
/// describes a different `signature_hash` than the graph now holds. The first
/// update after an upgrade therefore reads every file, whatever it was asked
/// about. Within the scope a body whose normalized text is unchanged keeps its
/// vector (the `embeddings.bin` v3 input-hash skip).
pub fn update_bodies(
    backend: &dyn GraphBackend,
    root: &Path,
    changed_files: &[&str],
    make_embedder: &dyn Fn() -> Box<dyn EmbedProvider>,
) -> Result<BuildReport> {
    let sidecar = root.join(".infigraph").join(BODIES_FILE);
    let wanted = parse(backend.raw_query(SYMBOLS)?);
    let recorded_hashes = backend.get_file_hashes()?;

    let loaded = if sidecar.exists() {
        bodies::load(&sidecar).unwrap_or_else(|e| {
            crate::last_run::note(
                "clone bodies file unreadable, rebuilt",
                1,
                &format!("{e:#}"),
            );
            Vec::new()
        })
    } else {
        Vec::new()
    };
    let mut entries: HashMap<String, BodyEntry> =
        loaded.iter().map(|e| (e.id.clone(), e.clone())).collect();

    let changed: HashSet<&str> = changed_files.iter().copied().collect();
    let mut by_file: HashMap<&str, Vec<&Wanted>> = HashMap::new();
    for w in &wanted {
        let current = entries
            .get(&w.id)
            .is_some_and(|e| e.signature == w.signature);
        if changed.is_empty() || changed.contains(w.file.as_str()) || !current {
            by_file.entry(w.file.as_str()).or_default().push(w);
        }
    }

    let files: Vec<(&str, Vec<&Wanted>)> = by_file.into_iter().collect();
    let results: Vec<(Vec<&Wanted>, Option<Vec<Prepared>>)> = files
        .into_par_iter()
        .map(|(file, symbols)| {
            let prepared = prepare_file(root, file, recorded_hashes.get(file), &symbols);
            (symbols, prepared)
        })
        .collect();

    let mut report = BuildReport {
        symbols: wanted.len(),
        ..BuildReport::default()
    };
    let mut to_embed: Vec<Prepared> = Vec::new();
    for (symbols, prepared) in results {
        let Some(prepared) = prepared else {
            report.files_changed_since_index += 1;
            continue;
        };
        // A body that now has no tokens has nothing to describe.
        let built: HashSet<&str> = prepared.iter().map(|p| p.id.as_str()).collect();
        for w in symbols {
            if !built.contains(w.id.as_str()) {
                entries.remove(&w.id);
            }
        }
        for p in prepared {
            match entries.get(&p.id) {
                Some(old) if old.input_hash == p.input_hash && !old.embedding.is_empty() => {
                    let old = &entries[&p.id];
                    let kept = BodyEntry {
                        id: p.id.clone(),
                        input_hash: p.input_hash,
                        signature: p.signature,
                        token_count: p.token_count,
                        sketch: p.sketch,
                        embedding: old.embedding.clone(),
                    };
                    entries.insert(p.id, kept);
                }
                _ => to_embed.push(p),
            }
        }
    }

    report.embed_requests = to_embed.len();
    if !to_embed.is_empty() {
        embed_into(&mut entries, to_embed, make_embedder);
    }

    let live_ids: HashSet<&str> = wanted.iter().map(|w| w.id.as_str()).collect();
    entries.retain(|id, _| live_ids.contains(id.as_str()));

    report.embedded = wanted
        .iter()
        .filter(|w| {
            entries
                .get(&w.id)
                .is_some_and(|e| e.signature == w.signature && !e.embedding.is_empty())
        })
        .count();
    if report.files_changed_since_index > 0 {
        crate::last_run::note(
            "files changed since they were indexed",
            report.files_changed_since_index as u64,
            "their functions get a clone-bodies entry after the next index",
        );
    }

    let mut after: Vec<BodyEntry> = entries.into_values().collect();
    after.sort_by(|a, b| a.id.cmp(&b.id));
    let mut before = loaded;
    before.sort_by(|a, b| a.id.cmp(&b.id));
    if after != before || !sidecar.exists() {
        bodies::save(&sidecar, &after)?;
    }

    if report.embedded < report.symbols {
        live::set(
            root,
            DegradedMode::CloneBodiesPartial {
                missing: report.symbols - report.embedded,
                total: report.symbols,
            },
        );
    } else {
        live::clear(root, CLONE_BODIES_PARTIAL);
    }
    Ok(report)
}

/// Embeds `prepared` and stores each as an entry. A body the embedder could
/// not embed is stored without a vector: its sketch is still its signal.
fn embed_into(
    entries: &mut HashMap<String, BodyEntry>,
    prepared: Vec<Prepared>,
    make_embedder: &dyn Fn() -> Box<dyn EmbedProvider>,
) {
    let embedder: Arc<Box<dyn EmbedProvider>> = Arc::new(make_embedder());
    let first_error: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let built: Vec<Vec<BodyEntry>> = prepared
        .par_chunks(BATCH)
        .map(|chunk| {
            let texts: Vec<&str> = chunk.iter().map(|p| p.text.as_str()).collect();
            let vectors = embedder.embed_batch(&texts).unwrap_or_else(|e| {
                if let Ok(mut first) = first_error.lock() {
                    first.get_or_insert_with(|| format!("{e:#}"));
                }
                Vec::new()
            });
            chunk
                .iter()
                .enumerate()
                .map(|(i, p)| BodyEntry {
                    id: p.id.clone(),
                    input_hash: p.input_hash,
                    signature: p.signature,
                    token_count: p.token_count,
                    sketch: p.sketch,
                    embedding: vectors.get(i).cloned().unwrap_or_default(),
                })
                .collect()
        })
        .collect();
    let mut unembedded = 0u64;
    for entry in built.into_iter().flatten() {
        unembedded += entry.embedding.is_empty() as u64;
        entries.insert(entry.id.clone(), entry);
    }
    if unembedded > 0 {
        let reason = first_error
            .into_inner()
            .ok()
            .flatten()
            .unwrap_or_else(|| "the embedder returned fewer vectors than asked".to_string());
        crate::last_run::note("function bodies not embedded", unembedded, &reason);
    }
}

/// [`update_bodies`] for a caller that has nowhere to return an error: the
/// embeddings update it rides on has already succeeded, and a failure here
/// costs the body signal, which `degraded` then reports.
pub fn record_update(
    backend: &dyn GraphBackend,
    root: &Path,
    changed_files: &[&str],
    make_embedder: &dyn Fn() -> Box<dyn EmbedProvider>,
) {
    if let Err(e) = update_bodies(backend, root, changed_files, make_embedder) {
        crate::last_run::note("clone bodies not updated", 1, &format!("{e:#}"));
    }
}
