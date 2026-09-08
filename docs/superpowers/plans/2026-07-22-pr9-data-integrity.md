# PR9: Data Integrity — Sidecar Atomicity, Format Header, Quarantine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the two remaining non-atomic sidecar writers (BM25 cache, HNSW index), add a corruption-detecting format header (magic + version + checksum) to the embeddings sidecar, and replace unconditional deletion with bounded quarantine for the two *automatic* (non-user-initiated) graph-recovery paths.

**Architecture:** Reuses the exact temp-file-then-`rename(2)` pattern already shipped for `embed::save_embeddings` (PR4) for the two remaining non-atomic sidecars. Adds a small, dependency-free format header (4-byte magic + 1-byte version + `std::collections::hash_map::DefaultHasher`-based checksum trailer) to `embeddings.bin`, with graceful fallback to the old headerless format on read so existing files keep working until their next write naturally upgrades them. Wires the already-existing but currently-unused `GraphCorruption` marker type into the two wipe paths that fire *without* a human in the loop (Kuzu-open-failure recovery in `Infigraph::init()`, and SIGSEGV auto-recovery in `infigraph-mcp`), renaming the doomed graph directory aside into a bounded quarantine pool instead of deleting it.

**Tech Stack:** Rust, `anyhow`, `std::fs::rename` for atomicity (no new crate dependencies — checksum uses `std::collections::hash_map::DefaultHasher`, already available via std).

## Global Constraints

- Every `cargo` invocation MUST be run with `CARGO_PROFILE_DEV_DEBUG=0` in the environment (repo-wide rule; mixing debug-info settings has caused ENOSPC incidents from duplicate C++ build trees on this machine this session).
- **Scope boundary, read carefully:** this plan touches only the two *automatic* recovery wipe paths — `Infigraph::wipe_graph` (`crates/infigraph-core/src/lib.rs:204-241`, called from `init()`'s Kuzu-open-failure branch) and `wipe_code_and_docs`/`wipe_code_and_docs_with_timeout` (`crates/infigraph-mcp/src/recovery.rs:10-46`, called from SIGSEGV auto-recovery). **`ops::wipe_infigraph_preserving_index_lock` (`crates/infigraph-core/src/ops.rs:88-101`, used by the user-invoked `infigraph index --full`) is explicitly OUT OF SCOPE for quarantine** — a `--full` reindex is deliberate, explicit user intent to rebuild from scratch, not automatic corruption recovery, so it should keep deleting, not quarantining. Do not touch it in this plan.
- **Also out of scope for this plan** (deferred to a PR9b follow-up, to be planned separately): pre-write snapshots + `infigraph restore` (R3.2.1/R3.2.2 — a distinct mechanism from quarantine: snapshots are periodic *known-good* backups you'd actually want to restore from; quarantine holds *corrupt* graphs for forensics, never for restoration), format headers for `bm25_cache.bin` and the HNSW `.meta` sidecar (only `embeddings.bin` gets the header in this plan — it's the highest-value target, shared by code/doc/session/combined-group embeddings), and graph generation counters / SCIP-staleness tracking (R3.3.3/R3.3.4).
- The existing `GraphCorruption` marker (`crates/infigraph-core/src/graph/store.rs:49-51`, doc comment already references "DESIGN-hardening.md R3.1... route to quarantine") currently has zero consumers anywhere in the codebase — this plan makes it the first real consumer. Do not introduce a second, parallel corruption-signal type.
- Quarantine retention is bounded at **N=2**: when a third quarantine directory would be created for the same project, delete the oldest one first (by directory name's embedded timestamp, not mtime) so the pool never grows unbounded.
- Quarantine directory naming: `graph.corrupt.<unix-epoch-seconds>/` as a sibling of the wiped path inside `.infigraph/` (e.g. `.infigraph/graph.corrupt.1784700000/`), matching the exact pattern named in the design doc (`docs/DESIGN-hardening.md`, R3.1.2: *"rename to `graph.corrupt.<ts>/`"*).

---

### Task 1: Atomic writes for the BM25 cache and HNSW sidecars

**Files:**
- Modify: `crates/infigraph-core/src/search/mod.rs:109-134` (`BM25Index::save`)
- Modify: `crates/infigraph-core/src/embed/mod.rs:671-761` (`build_hnsw_index`, `write_binary_sidecar`)
- Test: `crates/infigraph-core/tests/sidecar_atomicity.rs` (new file)

**Interfaces:**
- Consumes: nothing new — `BM25Index::save`'s signature (`pub fn save(&self, path: &Path) -> Result<()>`) and `build_hnsw_index`'s signature (`pub fn build_hnsw_index(embeddings: &[(String, Vec<f32>)], index_path: &Path, embeddings_path: &Path) -> Result<usize>`) are both unchanged — only their bodies change.
- Produces: both writers now leave a fully-formed file or none at all — no consumer-visible change beyond that guarantee.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/tests/sidecar_atomicity.rs`:

```rust
use infigraph_core::embed::build_hnsw_index;
use infigraph_core::search::BM25Index;

#[test]
fn bm25_save_leaves_no_temp_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bm25_cache.bin");
    let mut idx = BM25Index::default();
    idx.add_document("doc1".to_string(), "hello world".to_string());
    idx.save(&path).unwrap();
    assert!(path.exists());
    assert!(!dir.path().join("bm25_cache.bin.tmp").exists());
    let loaded = BM25Index::load(&path).unwrap();
    assert_eq!(loaded.doc_count(), 1);
}

#[test]
fn hnsw_build_leaves_no_temp_files_behind() {
    let dir = tempfile::tempdir().unwrap();
    let embeddings_path = dir.path().join("embeddings.bin");
    let index_path = dir.path().join("hnsw_index.usearch");
    let embeddings = vec![
        ("a".to_string(), vec![1.0_f32, 0.0, 0.0, 0.0]),
        ("b".to_string(), vec![0.0_f32, 1.0, 0.0, 0.0]),
    ];
    infigraph_core::embed::save_embeddings(&embeddings_path, &embeddings).unwrap();
    let n = build_hnsw_index(&embeddings, &index_path, &embeddings_path).unwrap();
    assert_eq!(n, 2);
    assert!(index_path.exists());
    let meta_path = index_path.with_extension("meta");
    assert!(meta_path.exists());
    assert!(!dir.path().join("hnsw_index.usearch.tmp").exists());
    assert!(!dir.path().join("hnsw_index.meta.tmp").exists());
}
```

Note: if `BM25Index` does not currently expose `add_document`/`doc_count`/`Default` publicly, use whatever its existing test helpers construct it with instead — check `crates/infigraph-core/src/search/mod.rs`'s own `#[cfg(test)] mod tests` block for the established construction pattern and match it exactly rather than guessing a new API surface.

- [ ] **Step 2: Run test to verify it fails or passes vacuously**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test sidecar_atomicity -- --nocapture`

Expected: both tests likely PASS today too (no temp files are ever created by the current direct-write implementations, so "no leftover temp file" is vacuously true pre-fix) — these become real regression guards once Step 3 introduces temp files. This mirrors the same TDD caveat already documented in this branch's `embed_atomicity.rs` tests (PR4 Task 1) — the meaningful assertion here is behavioral (file exists, loads correctly), not a race test, so there's no flakiness concern for these two.

- [ ] **Step 3: Make `BM25Index::save` atomic**

Replace the body of `save` in `crates/infigraph-core/src/search/mod.rs:109-134`:

```rust
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut buf = Vec::new();
        buf.push(1u8); // version
        buf.extend_from_slice(&self.avg_doc_len.to_le_bytes());
        buf.extend_from_slice(&(self.docs.len() as u32).to_le_bytes());
        for (id, text) in &self.docs {
            let id_b = id.as_bytes();
            buf.extend_from_slice(&(id_b.len() as u32).to_le_bytes());
            buf.extend_from_slice(id_b);
            let text_b = text.as_bytes();
            buf.extend_from_slice(&(text_b.len() as u32).to_le_bytes());
            buf.extend_from_slice(text_b);
        }
        buf.extend_from_slice(&(self.inverted.len() as u32).to_le_bytes());
        for (term, postings) in &self.inverted {
            let tb = term.as_bytes();
            buf.extend_from_slice(&(tb.len() as u32).to_le_bytes());
            buf.extend_from_slice(tb);
            buf.extend_from_slice(&(postings.len() as u32).to_le_bytes());
            for &(doc_idx, tf) in postings {
                buf.extend_from_slice(&(doc_idx as u32).to_le_bytes());
                buf.extend_from_slice(&tf.to_le_bytes());
            }
        }
        let tmp_path = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("bm25_cache.bin")
        ));
        std::fs::write(&tmp_path, &buf)
            .map_err(|e| anyhow::anyhow!("write bm25 cache temp file: {}", e))?;
        std::fs::rename(&tmp_path, path)
            .map_err(|e| anyhow::anyhow!("atomically replace bm25 cache: {}", e))?;
        Ok(())
    }
```

- [ ] **Step 4: Make `build_hnsw_index`/`write_binary_sidecar` atomic**

In `crates/infigraph-core/src/embed/mod.rs`, replace the `.save`/sidecar-write portion of `build_hnsw_index` (currently lines 713-725):

```rust
    let path_str = index_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 index path"))?;
    let tmp_index_path = index_path.with_file_name(format!(
        "{}.tmp",
        index_path.file_name().and_then(|n| n.to_str()).unwrap_or("hnsw_index.usearch")
    ));
    let tmp_index_str = tmp_index_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-utf8 temp index path"))?;
    index
        .save(tmp_index_str)
        .map_err(|e| anyhow::anyhow!("usearch save: {e}"))?;
    std::fs::rename(&tmp_index_path, index_path)
        .context("atomically replace hnsw index file")?;

    let emb_mtime = std::fs::metadata(embeddings_path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH);
    let sidecar_path = index_path.with_extension("meta");
    write_binary_sidecar(&sidecar_path, emb_mtime, n, dim, embeddings)?;

    invalidate_hnsw_cache();
    Ok(n)
}
```

(The `path_str` variable is no longer used since `index.save` now targets the temp path — remove the now-dead `let path_str = ...` binding shown above the diff context; keep everything from `let dim = ...` through the thread-scoped index-build unchanged.)

Replace the write call at the end of `write_binary_sidecar` (currently `crates/infigraph-core/src/embed/mod.rs:759`, `std::fs::write(path, &buf).context("write binary hnsw sidecar")?;`):

```rust
    let tmp_path = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("hnsw.meta")
    ));
    std::fs::write(&tmp_path, &buf).context("write binary hnsw sidecar temp file")?;
    std::fs::rename(&tmp_path, path).context("atomically replace binary hnsw sidecar")?;
    Ok(())
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test sidecar_atomicity -- --nocapture`

Expected: both tests `PASS`. Also run the existing suites that exercise these two writers to confirm no regression: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --lib search:: -- --nocapture` and `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --lib embed:: -- --nocapture`.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/search/mod.rs crates/infigraph-core/src/embed/mod.rs crates/infigraph-core/tests/sidecar_atomicity.rs
git commit -m "fix: BM25 cache and HNSW index/meta sidecars write atomically via temp+rename (R3.3.1)"
```

---

### Task 2: Corruption-detecting format header for `embeddings.bin`

**Files:**
- Modify: `crates/infigraph-core/src/embed/mod.rs:388-439` (`save_embeddings`, `load_embeddings`)
- Test: `crates/infigraph-core/tests/embed_atomicity.rs` (extend — this is the file Task 1 of PR4 created)

**Interfaces:**
- Consumes: nothing new.
- Produces: `save_embeddings`/`load_embeddings` signatures unchanged. New on-disk format is transparent to every caller (code/doc/session/combined-group embeddings all go through this one function, per PR9's own research — no other file needs changes for this task).

- [ ] **Step 1: Write the failing test**

Add to `crates/infigraph-core/tests/embed_atomicity.rs`:

```rust
#[test]
fn save_embeddings_detects_truncation_via_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("embeddings.bin");
    let data = vec![
        ("a".to_string(), vec![1.0, 2.0, 3.0, 4.0]),
        ("b".to_string(), vec![5.0, 6.0, 7.0, 8.0]),
    ];
    save_embeddings(&path, &data).unwrap();

    // Corrupt the file: flip a byte in the middle of the payload without
    // changing its length, so length-based `ensure!` checks in the parser
    // would not catch it — only a checksum can.
    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let result = load_embeddings(&path);
    assert!(
        result.is_err(),
        "a single flipped byte must be caught by the checksum, not silently parsed"
    );
}

#[test]
fn load_embeddings_still_reads_pre_header_legacy_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("embeddings.bin");
    // Hand-write the OLD headerless format directly (no magic/version/checksum)
    // to simulate a file written before this task landed.
    let mut buf = Vec::new();
    buf.extend_from_slice(&1u32.to_le_bytes()); // count
    let id = b"legacy";
    buf.extend_from_slice(&(id.len() as u32).to_le_bytes());
    buf.extend_from_slice(id);
    buf.extend_from_slice(&2u32.to_le_bytes()); // dim
    buf.extend_from_slice(&1.0_f32.to_le_bytes());
    buf.extend_from_slice(&2.0_f32.to_le_bytes());
    std::fs::write(&path, &buf).unwrap();

    let loaded = load_embeddings(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].0, "legacy");
    assert_eq!(loaded[0].1, vec![1.0, 2.0]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test embed_atomicity -- --nocapture`

Expected: `save_embeddings_detects_truncation_via_checksum` FAILS today — the current format has no checksum, so a single flipped byte inside a float or an id string is silently parsed as different (garbage) data rather than rejected; `result.is_err()` is false. `load_embeddings_still_reads_pre_header_legacy_files` PASSES today trivially (it's testing the current-format read path against current-format bytes) — it becomes a real backward-compatibility regression guard once Step 3 changes the format.

- [ ] **Step 3: Add magic + version + checksum header**

Replace `save_embeddings` (`crates/infigraph-core/src/embed/mod.rs:388-409`, the version already made atomic by PR4 Task 1):

```rust
const EMBEDDINGS_MAGIC: [u8; 4] = *b"IGE1";
const EMBEDDINGS_FORMAT_VERSION: u8 = 2;

/// Save symbol embeddings to a binary file. Format:
/// [magic:4][version:u8][count:u32] then per entry [id_len:u32][id_bytes][dim:u32][f32*dim],
/// followed by an 8-byte trailing checksum (std DefaultHasher) over everything
/// from `count` through the last entry (i.e. everything after the magic+version).
pub fn save_embeddings(path: &Path, embeddings: &[(String, Vec<f32>)]) -> Result<()> {
    use std::hash::{Hash, Hasher};
    let tmp_path = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("embeddings.bin")
    ));
    {
        let file = std::fs::File::create(&tmp_path).context("create temp embeddings file")?;
        let mut w = BufWriter::new(file);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        w.write_all(&EMBEDDINGS_MAGIC)?;
        w.write_all(&[EMBEDDINGS_FORMAT_VERSION])?;

        let count_bytes = (embeddings.len() as u32).to_le_bytes();
        w.write_all(&count_bytes)?;
        count_bytes.hash(&mut hasher);

        for (id, vec) in embeddings {
            let id_bytes = id.as_bytes();
            let id_len_bytes = (id_bytes.len() as u32).to_le_bytes();
            w.write_all(&id_len_bytes)?;
            id_len_bytes.hash(&mut hasher);
            w.write_all(id_bytes)?;
            id_bytes.hash(&mut hasher);
            let dim_bytes = (vec.len() as u32).to_le_bytes();
            w.write_all(&dim_bytes)?;
            dim_bytes.hash(&mut hasher);
            for &v in vec {
                let f_bytes = v.to_le_bytes();
                w.write_all(&f_bytes)?;
                f_bytes.hash(&mut hasher);
            }
        }

        w.write_all(&hasher.finish().to_le_bytes())?;
        w.flush().context("flush temp embeddings file")?;
    }
    std::fs::rename(&tmp_path, path).context("atomically replace embeddings file")?;
    invalidate_embeddings_cache();
    Ok(())
}
```

Replace `load_embeddings` (`crates/infigraph-core/src/embed/mod.rs:412-...`, ending wherever its current closing brace is):

```rust
/// Load symbol embeddings from a binary file using memory-mapped I/O.
/// Transparently reads both the current header+checksum format and the
/// legacy headerless format written before this format existed.
pub fn load_embeddings(path: &Path) -> Result<Vec<(String, Vec<f32>)>> {
    use std::hash::{Hash, Hasher};
    let file = std::fs::File::open(path).context("open embeddings file")?;
    let mmap = unsafe { memmap2::Mmap::map(&file) }.context("mmap embeddings file")?;
    let data = &mmap[..];

    anyhow::ensure!(data.len() >= 4, "embeddings file too small");

    let (payload, has_checksum) = if data.len() >= 5 && data[0..4] == EMBEDDINGS_MAGIC {
        anyhow::ensure!(
            data.len() >= 5 + 8,
            "embeddings file has a header but is too small to hold a checksum trailer"
        );
        let version = data[4];
        anyhow::ensure!(
            version == EMBEDDINGS_FORMAT_VERSION,
            "unsupported embeddings format version: {version}"
        );
        let payload_end = data.len() - 8;
        let payload = &data[5..payload_end];
        let stored_checksum = u64::from_le_bytes(data[payload_end..].try_into().unwrap());
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        payload.hash(&mut hasher);
        anyhow::ensure!(
            hasher.finish() == stored_checksum,
            "embeddings file checksum mismatch — file is corrupt: {}",
            path.display()
        );
        (payload, true)
    } else {
        (data, false)
    };
    let _ = has_checksum; // only used to document the branch above; no further behavior differs

    anyhow::ensure!(payload.len() >= 4, "embeddings payload too small");
    let count = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    let mut result = Vec::with_capacity(count);
    let mut pos = 4usize;

    for _ in 0..count {
        anyhow::ensure!(pos + 4 <= payload.len(), "truncated embeddings file");
        let id_len = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        anyhow::ensure!(pos + id_len <= payload.len(), "truncated embeddings file");
        let id = std::str::from_utf8(&payload[pos..pos + id_len])
            .context("invalid utf8 in embedding id")?
            .to_string();
        pos += id_len;
        anyhow::ensure!(pos + 4 <= payload.len(), "truncated embeddings file");
        let dim = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let float_bytes = dim * 4;
        anyhow::ensure!(pos + float_bytes <= payload.len(), "truncated embeddings file");
        let vec: Vec<f32> = payload[pos..pos + float_bytes]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        pos += float_bytes;
        result.push((id, vec));
    }

    Ok(result)
}
```

Note: this plan writes `load_embeddings`'s tail (the per-entry parse loop) as a reconstruction from the pre-existing logic operating on `payload` instead of `data` — before editing, read the current full body of `load_embeddings` (`crates/infigraph-core/src/embed/mod.rs`, starts at line 412) to confirm the exact parse-loop code being replaced matches what's shown here (it should, per this plan's research, but confirm the closing brace and any trailing logic after the loop — e.g. a final `Ok(result)` — line up exactly before replacing).

- [ ] **Step 4: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test embed_atomicity -- --nocapture`

Expected: all four tests in this file `PASS` (the two new ones from this task, plus the two from PR4 Task 1 — confirming the header addition didn't break atomicity or the torn-write guarantee). Also run `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --lib embed:: -- --nocapture` and `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib tools::session:: -- --nocapture` (the session-embeddings consumer from PR4 Tasks 2-3) to confirm no downstream regression from every caller of `save_embeddings`/`load_embeddings` across the codebase.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/embed/mod.rs crates/infigraph-core/tests/embed_atomicity.rs
git commit -m "feat: embeddings.bin gets a magic+version+checksum header, detects corruption on load (R3.3.2/R8.1)"
```

---

### Task 3: Quarantine instead of delete for automatic graph recovery

**Files:**
- Create: `crates/infigraph-core/src/quarantine.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (add `pub mod quarantine;`; rewrite `wipe_graph`, `crates/infigraph-core/src/lib.rs:204-241`)
- Modify: `crates/infigraph-mcp/src/recovery.rs:14-46` (`wipe_code_and_docs_with_timeout`)
- Test: `crates/infigraph-core/tests/quarantine.rs` (new file)

**Interfaces:**
- Produces: `pub fn quarantine::quarantine_graph(infigraph_dir: &Path, graph_name: &str) -> Result<PathBuf>` — renames `<infigraph_dir>/<graph_name>` (plus its `.wal` and `.wal.*` siblings) to `<infigraph_dir>/<graph_name>.corrupt.<epoch_secs>/`, evicts the oldest quarantine directory first if this would be the 3rd (bounded N=2), and returns the new quarantine path. `graph_name` is `"graph"` for the code graph and `"docs.kuzu"`/whatever `recovery.rs` uses for the docs store — confirm the exact name `wipe_code_and_docs` uses before calling this (it currently only handles the code graph at `.infigraph/graph`, per this plan's research — the docs store cleanup goes through a separate `DocIndex::clean()` call that this task does not touch).
- Consumes (Task 3 only, not earlier tasks): nothing new from Tasks 1-2.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/tests/quarantine.rs`:

```rust
use infigraph_core::quarantine::quarantine_graph;
use std::fs;

#[test]
fn quarantine_renames_instead_of_deleting() {
    let dir = tempfile::tempdir().unwrap();
    let ig = dir.path().join(".infigraph");
    fs::create_dir_all(ig.join("graph")).unwrap();
    fs::write(ig.join("graph").join("catalog.kz"), b"fake db content").unwrap();

    let quarantine_path = quarantine_graph(&ig, "graph").unwrap();

    assert!(!ig.join("graph").exists(), "original graph dir must be gone from its live path");
    assert!(quarantine_path.exists(), "quarantine dir must exist");
    assert!(
        quarantine_path.join("catalog.kz").exists(),
        "quarantined content must be preserved, not deleted"
    );
    assert_eq!(
        fs::read(quarantine_path.join("catalog.kz")).unwrap(),
        b"fake db content"
    );
}

#[test]
fn quarantine_evicts_oldest_beyond_bound_of_two() {
    let dir = tempfile::tempdir().unwrap();
    let ig = dir.path().join(".infigraph");
    fs::create_dir_all(&ig).unwrap();

    // Pre-seed two quarantine dirs with distinct, ordered timestamps so
    // eviction order is deterministic regardless of wall-clock speed.
    fs::create_dir_all(ig.join("graph.corrupt.100")).unwrap();
    fs::create_dir_all(ig.join("graph.corrupt.200")).unwrap();

    fs::create_dir_all(ig.join("graph")).unwrap();
    fs::write(ig.join("graph").join("marker"), b"third").unwrap();
    let third = quarantine_graph(&ig, "graph").unwrap();

    assert!(
        !ig.join("graph.corrupt.100").exists(),
        "oldest quarantine dir (100) must be evicted to keep the bound at N=2"
    );
    assert!(
        ig.join("graph.corrupt.200").exists(),
        "newer pre-existing quarantine dir (200) must survive"
    );
    assert!(third.exists(), "the newly quarantined dir must exist");

    let remaining: Vec<_> = fs::read_dir(&ig)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("graph.corrupt."))
        .collect();
    assert_eq!(remaining.len(), 2, "quarantine pool must never exceed N=2");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test quarantine -- --nocapture`

Expected: FAIL to compile — `infigraph_core::quarantine` doesn't exist yet. This is the expected RED state for a new module.

- [ ] **Step 3: Implement `quarantine_graph`**

Create `crates/infigraph-core/src/quarantine.rs`:

```rust
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// How many quarantined copies of a given graph name to retain. When a new
/// quarantine would exceed this, the oldest (by embedded timestamp, not
/// filesystem mtime) is deleted first.
const QUARANTINE_RETENTION: usize = 2;

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Rename a corrupt graph directory (and its WAL-family siblings) aside into
/// a bounded quarantine pool instead of deleting it, per
/// docs/DESIGN-hardening.md R3.1.2. `infigraph_dir` is the `.infigraph/`
/// directory; `graph_name` is the base name of the graph within it (e.g.
/// `"graph"`). Returns the path the graph was moved to.
///
/// Callers are responsible for holding whatever write lock guards
/// `graph_name` before calling this — quarantine itself does not lock,
/// mirroring `wipe_graph`'s existing contract where the caller already
/// acquired `graph.lock` before deciding to wipe.
pub fn quarantine_graph(infigraph_dir: &Path, graph_name: &str) -> Result<PathBuf> {
    evict_oldest_if_at_bound(infigraph_dir, graph_name)?;

    let ts = now_epoch_secs();
    let quarantine_path = infigraph_dir.join(format!("{graph_name}.corrupt.{ts}"));
    let source = infigraph_dir.join(graph_name);

    std::fs::rename(&source, &quarantine_path)
        .with_context(|| format!("quarantine: rename {} to {}", source.display(), quarantine_path.display()))?;

    // Move WAL-family siblings alongside the quarantined graph so a future
    // investigation has the full picture, not just the base image.
    let wal = infigraph_dir.join(format!("{graph_name}.wal"));
    if wal.exists() {
        let _ = std::fs::rename(&wal, quarantine_path.join(format!("{graph_name}.wal")));
    }
    if let Ok(entries) = std::fs::read_dir(infigraph_dir) {
        let prefix = format!("{graph_name}.wal.");
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                let _ = std::fs::rename(e.path(), quarantine_path.join(&name));
            }
        }
    }

    Ok(quarantine_path)
}

fn evict_oldest_if_at_bound(infigraph_dir: &Path, graph_name: &str) -> Result<()> {
    let prefix = format!("{graph_name}.corrupt.");
    let mut existing: Vec<(u64, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(infigraph_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(ts_str) = name.strip_prefix(&prefix) {
                if let Ok(ts) = ts_str.parse::<u64>() {
                    existing.push((ts, e.path()));
                }
            }
        }
    }
    if existing.len() < QUARANTINE_RETENTION {
        return Ok(());
    }
    existing.sort_by_key(|(ts, _)| *ts);
    // existing.len() >= QUARANTINE_RETENTION and we're about to add one more,
    // so evict enough of the oldest entries to land at QUARANTINE_RETENTION - 1
    // before the new one is created (bringing the total back to QUARANTINE_RETENTION).
    let to_evict = existing.len() - (QUARANTINE_RETENTION - 1);
    for (_, path) in existing.into_iter().take(to_evict) {
        let _ = std::fs::remove_dir_all(&path);
    }
    Ok(())
}
```

Add `pub mod quarantine;` to `crates/infigraph-core/src/lib.rs`'s module list (alongside the existing `pub mod lockfile;`, `pub mod ops;`, etc. — match the existing alphabetical/grouping convention in that file).

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test quarantine -- --nocapture`

Expected: both tests `PASS`.

- [ ] **Step 5: Wire quarantine into `Infigraph::wipe_graph`**

Replace `wipe_graph` in `crates/infigraph-core/src/lib.rs:204-241`:

```rust
    fn wipe_graph(db_path: &Path) -> Result<()> {
        // A wipe must never race a live writer: take the same per-graph lock
        // writers hold. Busy here means a live process -- refuse, don't destroy.
        let lock_path = db_path.with_extension("lock");
        let _lock =
            crate::lockfile::acquire(&lock_path, "graph-wipe", std::time::Duration::from_secs(5))?;

        // Quarantine (rename aside) instead of delete: this path only runs
        // after init()'s retry/backoff verdict concludes the graph is
        // durably unopenable and no live process holds it (see the caller).
        // Per R3.1.2, an automatic corruption verdict must not destroy the
        // evidence -- a human may need it to diagnose what went wrong.
        if let (Some(parent), Some(name)) = (db_path.parent(), db_path.file_name()) {
            if db_path.exists() {
                let _ = crate::quarantine::quarantine_graph(parent, &name.to_string_lossy());
            }
        }
        // quarantine_graph best-effort-moves the base path + `.wal`/`.wal.*`
        // family together; fall through to remove anything it couldn't move
        // (e.g. if db_path didn't exist, or a partial failure left remnants)
        // so a freshly recreated database never inherits stale WAL state.
        let _ = std::fs::remove_dir_all(db_path);
        let _ = std::fs::remove_file(db_path);
        let wal = PathBuf::from(format!("{}.wal", db_path.display()));
        let _ = std::fs::remove_file(&wal);
        if let (Some(parent), Some(name)) = (db_path.parent(), db_path.file_name()) {
            let prefix = format!("{}.wal.", name.to_string_lossy());
            if let Ok(entries) = std::fs::read_dir(parent) {
                for e in entries.flatten() {
                    if e.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
        Ok(())
    }
```

- [ ] **Step 6: Wire quarantine into `wipe_code_and_docs_with_timeout`**

Replace the graph-wipe portion of `wipe_code_and_docs_with_timeout` in `crates/infigraph-mcp/src/recovery.rs:14-46` (the `let graph_path = ig.join("graph");` block through the WAL-family cleanup, leaving the `_lock` acquisition above it and the `DocIndex` cleanup below it unchanged):

```rust
    let graph_path = ig.join("graph");
    if graph_path.exists() {
        let _ = infigraph_core::quarantine::quarantine_graph(&ig, "graph");
    }
    let _ = std::fs::remove_file(&graph_path);
    let _ = std::fs::remove_dir_all(&graph_path);
    let _ = std::fs::remove_file(ig.join("graph.wal"));
    // Also remove Kuzu's WAL-family temp siblings (e.g. graph.wal.checkpoint):
    // one left behind carries the old database's ID and permanently blocks
    // opening a freshly rebuilt graph.
    if let Ok(entries) = std::fs::read_dir(&ig) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("graph.wal.") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
```

- [ ] **Step 7: Write regression tests confirming both wipe paths quarantine, not delete**

Add to the existing `#[cfg(test)] mod tests` block in `crates/infigraph-core/src/lib.rs` (colocated with `init_wipes_and_rebuilds_on_persistent_corruption`, which this test extends and reuses the same setup pattern from — both need the same test-only imports already present in that module: `GraphStore`, `LanguageRegistry`, `tempfile::TempDir`):

```rust
    #[test]
    fn init_quarantines_instead_of_deleting_on_persistent_corruption() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let db_path = root.join(".infigraph").join("graph");

        {
            let store = GraphStore::open(&db_path).unwrap();
            let conn = store.connection().unwrap();
            conn.query(
                "CREATE (:Symbol {id: 'marker::quarantined', name: 'quarantined', kind: 'function', \
                 file: 'marker.rs', start_line: 0, end_line: 0, signature_hash: '', \
                 language: 'rust', visibility: 'public', parent: '', docstring: '', \
                 complexity: 0, parameters: '', return_type: ''})",
            )
            .unwrap();
        }
        // Corrupt permanently -- nothing heals this one.
        std::fs::write(&db_path, b"not a valid kuzu database file at all").unwrap();

        let registry = LanguageRegistry::new();
        let mut ig = Infigraph::open(root, registry).unwrap();
        let result = ig.init();
        assert!(result.is_ok(), "init() must still recover: {result:?}");

        let infigraph_dir = root.join(".infigraph");
        let quarantine_dirs: Vec<_> = std::fs::read_dir(&infigraph_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("graph.corrupt."))
            .collect();
        assert_eq!(
            quarantine_dirs.len(),
            1,
            "exactly one quarantine dir must exist after a persistent-corruption wipe"
        );
        let quarantined_content =
            std::fs::read(quarantine_dirs[0].path()).unwrap_or_default();
        assert_eq!(
            quarantined_content, b"not a valid kuzu database file at all",
            "the quarantined file must preserve the exact corrupt content, not be re-created empty"
        );
        assert!(
            !db_path.exists() || GraphStore::open(&db_path).is_ok(),
            "the live db_path must either be gone or be the freshly rebuilt, openable database \
             -- never the old corrupt content left in place"
        );
    }
```

Note: `db_path` in this test's setup is a *file* written by `std::fs::write` (simulating Kuzu's corrupted single-file state at the point `wipe_graph` runs, matching the pre-existing test's own corruption technique exactly) rather than a directory — `quarantine_graph`'s `std::fs::rename` handles both files and directories identically (POSIX `rename(2)` doesn't care), so the assertion above reads the quarantined path as a file, not a directory listing. If `GraphStore::open`'s real corruption path produces a directory instead of a single file in practice, adjust the read accordingly, but do not change the underlying `quarantine_graph` implementation to special-case one over the other — it must handle either.

- [ ] **Step 8: Run tests to verify they pass**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --lib -- --nocapture` (covers `init_wipes_and_rebuilds_on_persistent_corruption` and the new companion test together) and `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --test quarantine -- --nocapture`. Also run `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --lib recovery:: -- --nocapture` (or wherever `wipe_code_and_docs`'s existing tests live — search for `test_wipe_refuses_while_graph_lock_held`, mentioned in this plan's research, to find the right module) to confirm the SIGSEGV-recovery path's existing tests still pass with quarantine wired in.

- [ ] **Step 9: Commit**

```bash
git add crates/infigraph-core/src/quarantine.rs crates/infigraph-core/src/lib.rs crates/infigraph-mcp/src/recovery.rs crates/infigraph-core/tests/quarantine.rs
git commit -m "feat: automatic graph recovery quarantines instead of deletes corrupt data, bounded N=2 (R3.1.1/R3.1.2)"
```

---

### Task 4: Full-suite verification

**Files:** none new

- [ ] **Step 1: Run the full `infigraph-core` suite**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-core --no-fail-fast -- --test-threads=4`

Expected: all green, no new failures relative to the pre-PR9 baseline.

- [ ] **Step 2: Run the full `infigraph-mcp` suite**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo test -p infigraph-mcp --no-fail-fast -- --test-threads=4`

Expected: all green except the two already-catalogued pre-existing failures (`tool_parity::advertised_tools_match_mcp_tool_names`, `watcher_concurrency::test_graph_tools_with_group_watchers`) — do not chase those, they predate this branch.

- [ ] **Step 3: Run clippy on the touched crates**

Run: `CARGO_PROFILE_DEV_DEBUG=0 cargo clippy -p infigraph-core -p infigraph-mcp -- -D warnings`

Expected: clean.

- [ ] **Step 4: Commit if Steps 1-3 required any fixes, otherwise stop here**

If Steps 1-3 were clean, there is nothing to commit for this task.
