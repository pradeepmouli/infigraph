//! `.infigraph/clone-bodies.bin`: what clone detection knows about each
//! function and method body.
//!
//! A regenerable sidecar, like `embeddings.bin`: nothing in it is the only
//! copy of anything, so a missing or unreadable file only costs the body
//! signal until the next `infigraph index`. Format (little endian):
//!
//! ```text
//! [magic "IGB1"][version u8][count u32]
//! per entry: [id_len u32][id][input_hash u64][signature u64][tokens u32]
//!            [sketch: 64 x u32][dim u32][f32 x dim]
//! [fnv1a64 of everything after the version byte, u64]
//! ```
//!
//! `input_hash` is the FNV-1a of the text the embedding was made from: an
//! entry whose body text is unchanged keeps its vector and is not embedded
//! again (the same skip `embeddings.bin` v3 makes). `signature` is the FNV-1a
//! of the graph's `signature_hash` for the symbol when the entry was built, so
//! a reader can tell an entry that no longer describes the symbol from one
//! that does.

use std::path::Path;

use anyhow::{Context, Result};

use super::sketch::{Sketch, SKETCH_LEN};
use crate::byte_reader::ByteReader;
use crate::embed::{atomic_write, fnv1a64};

/// The file's name under `.infigraph/`.
pub const BODIES_FILE: &str = "clone-bodies.bin";

const MAGIC: [u8; 4] = *b"IGB1";
/// A reader refuses any other version rather than guess, and the builder then
/// rebuilds the file. Bump it whenever the text a body is embedded from, or the
/// tokens its sketch is made of, would change for an unchanged body: an old
/// entry would otherwise be compared against new ones. v2: callee, member and
/// type names are kept in the normalization (v1 replaced every identifier).
pub const FORMAT_VERSION: u8 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct BodyEntry {
    pub id: String,
    pub input_hash: u64,
    pub signature: u64,
    /// Normalized tokens in the body: what `[clones] min_tokens` measures.
    pub token_count: u32,
    pub sketch: Sketch,
    /// Empty when no embedder could embed the body: the sketch is then the
    /// whole of this symbol's signal.
    pub embedding: Vec<f32>,
}

/// The value `BodyEntry::signature` holds for a graph `signature_hash`.
pub fn signature_of(signature_hash: &str) -> u64 {
    fnv1a64(signature_hash.as_bytes())
}

pub fn save(path: &Path, entries: &[BodyEntry]) -> Result<()> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        payload.extend_from_slice(&(entry.id.len() as u32).to_le_bytes());
        payload.extend_from_slice(entry.id.as_bytes());
        payload.extend_from_slice(&entry.input_hash.to_le_bytes());
        payload.extend_from_slice(&entry.signature.to_le_bytes());
        payload.extend_from_slice(&entry.token_count.to_le_bytes());
        for slot in entry.sketch {
            payload.extend_from_slice(&slot.to_le_bytes());
        }
        payload.extend_from_slice(&(entry.embedding.len() as u32).to_le_bytes());
        for v in &entry.embedding {
            payload.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut file = Vec::with_capacity(payload.len() + 13);
    file.extend_from_slice(&MAGIC);
    file.push(FORMAT_VERSION);
    file.extend_from_slice(&payload);
    file.extend_from_slice(&fnv1a64(&payload).to_le_bytes());
    atomic_write(path, &file).context("write the clone bodies sidecar")
}

/// Every entry in the file, or an error naming what is wrong with it. A
/// corrupt file is an error, never a panic or a partial result.
pub fn load(path: &Path) -> Result<Vec<BodyEntry>> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    anyhow::ensure!(
        data.len() >= MAGIC.len() + 1 + 8 && data[..4] == MAGIC,
        "{} is not a clone bodies file",
        path.display()
    );
    anyhow::ensure!(
        data[4] == FORMAT_VERSION,
        "{} has unsupported format version {}",
        path.display(),
        data[4]
    );
    let (payload, trailer) = data[5..].split_at(data.len() - 5 - 8);
    anyhow::ensure!(
        fnv1a64(payload).to_le_bytes() == trailer,
        "{} fails its checksum: the file is corrupt",
        path.display()
    );

    // An entry is at least its fixed fields with an empty id and vector.
    const MIN_ENTRY_BYTES: usize = 4 + 8 + 8 + 4 + 4 * SKETCH_LEN + 4;
    let mut r = ByteReader::new(payload, "clone bodies file");
    let count = r.count(MIN_ENTRY_BYTES)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let id = r.utf8_u32()?;
        let input_hash = r.u64()?;
        let signature = r.u64()?;
        let token_count = r.u32()?;
        let mut sketch = [0u32; SKETCH_LEN];
        for slot in &mut sketch {
            *slot = r.u32()?;
        }
        let dim = r.u32()? as usize;
        let float_bytes = dim
            .checked_mul(4)
            .context("clone bodies file: a dimension overflows")?;
        #[allow(clippy::chunks_exact_to_as_chunks)]
        let embedding = r
            .bytes(float_bytes)?
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        entries.push(BodyEntry {
            id,
            input_hash,
            signature,
            token_count,
            sketch,
            embedding,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, dim: usize) -> BodyEntry {
        BodyEntry {
            id: id.to_string(),
            input_hash: 7,
            signature: signature_of("abc"),
            token_count: 42,
            sketch: std::array::from_fn(|i| i as u32 * 3),
            embedding: (0..dim).map(|i| i as f32 * 0.5).collect(),
        }
    }

    fn path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join(BODIES_FILE)
    }

    #[test]
    fn entries_survive_a_round_trip_with_and_without_an_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let entries = vec![entry("a.rs::f", 8), entry("b.rs::g", 0)];
        save(&path(&dir), &entries).unwrap();
        assert_eq!(load(&path(&dir)).unwrap(), entries);
    }

    #[test]
    fn an_empty_file_of_entries_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        save(&path(&dir), &[]).unwrap();
        assert!(load(&path(&dir)).unwrap().is_empty());
    }

    #[test]
    fn a_flipped_byte_fails_the_checksum() {
        let dir = tempfile::tempdir().unwrap();
        save(&path(&dir), &[entry("a.rs::f", 4)]).unwrap();
        let mut bytes = std::fs::read(path(&dir)).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        std::fs::write(path(&dir), bytes).unwrap();
        assert!(format!("{:#}", load(&path(&dir)).unwrap_err()).contains("checksum"));
    }

    #[test]
    fn a_truncated_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        save(&path(&dir), &[entry("a.rs::f", 4)]).unwrap();
        let bytes = std::fs::read(path(&dir)).unwrap();
        for keep in [0, 3, 5, 12, bytes.len() - 1] {
            std::fs::write(path(&dir), &bytes[..keep]).unwrap();
            assert!(load(&path(&dir)).is_err(), "kept {keep} bytes");
        }
    }

    #[test]
    fn a_count_the_bytes_cannot_hold_is_refused_before_it_allocates() {
        let dir = tempfile::tempdir().unwrap();
        let mut payload = u32::MAX.to_le_bytes().to_vec();
        payload.extend_from_slice(&[0; 16]);
        let mut file = MAGIC.to_vec();
        file.push(FORMAT_VERSION);
        file.extend_from_slice(&payload);
        file.extend_from_slice(&fnv1a64(&payload).to_le_bytes());
        std::fs::write(path(&dir), file).unwrap();
        assert!(load(&path(&dir)).is_err());
    }

    #[test]
    fn another_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        save(&path(&dir), &[]).unwrap();
        let mut bytes = std::fs::read(path(&dir)).unwrap();
        bytes[4] = FORMAT_VERSION + 1;
        std::fs::write(path(&dir), bytes).unwrap();
        assert!(format!("{:#}", load(&path(&dir)).unwrap_err()).contains("version"));
    }
}
