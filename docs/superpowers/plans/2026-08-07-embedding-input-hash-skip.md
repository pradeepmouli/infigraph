# Symbol-Level Embedding Skip Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `update_embeddings` re-embeds a symbol only when its embedding input actually changed, and skips the `save_embeddings` write and O(n) HNSW rebuild entirely when nothing changed — making body-only edits (the most common watcher event) a pure hash check.

**Architecture:** `embeddings.bin` gains format version 3 whose entries carry `input_hash: u64` (FNV-1a of the exact `rich_symbol_text_full` string that produced the vector; 0 reserved as "unknown"). `save_embeddings`'s public signature is untouched — it has 39 references across four crates (doc/session/combined embeddings share the format) and keeps writing v2. Only `update_embeddings` writes v3 via a new `save_embeddings_hashed`. One shared parser handles legacy/v2/v3.

**Tech Stack:** Rust, `crates/infigraph-core` only. Branch: `feat/hardening` (depends on the hardening-only magic+version+checksum header).

**Spec:** `docs/superpowers/specs/2026-08-07-doc-bm25-cache-and-embedding-skip-design.md`

## Global Constraints

- Branch: `feat/hardening`, main checkout (no worktree needed).
- `save_embeddings(path, &[(String, Vec<f32>)])` and `load_embeddings(path) -> Result<Vec<(String, Vec<f32>)>>` keep their exact signatures and keep writing/reading as today (v2 out, legacy/v2/v3 in).
- `update_embeddings(backend, root, changed_files) -> Result<usize>` keeps its exact signature and return semantics (total embedding count).
- Checksum discipline: hash payload bytes via `Hasher::write` (raw), never the `Hash` trait's `.hash()` — see the NOTE comment in `save_embeddings` (`crates/infigraph-core/src/embed/mod.rs:479`).
- Unknown version bytes must still be rejected (`health_signals.rs` contract).
- Disk is tight: test per-crate (`cargo test -p infigraph-core --test <name>`), not `--all` in one shot; finish with one `cargo test --all` pass only at the very end.
- `env -u INFIGRAPH_WATCH_DAEMON` prefix for any test run that spawns watchers (user's ~/.zshrc leaks it); the tests below don't spawn watchers, but the final `--all` pass does.

---

### Task 1: v3 format — `fnv1a64`, versioned header helper, hashed save/load

**Files:**
- Modify: `crates/infigraph-core/src/embed/mod.rs` — constants at L447-448, `embeddings_count_offset` at L462-473, `save_embeddings` at L479-524 (unchanged body, add sibling), `load_embeddings` at L529-597 (refactor to delegate)
- Test: Create `crates/infigraph-core/tests/embed_hash_format.rs`

**Interfaces:**
- Consumes: existing `EMBEDDINGS_MAGIC: [u8; 4] = *b"IGE1"`, `EMBEDDINGS_FORMAT_VERSION: u8 = 2`, `atomic_tmp_path`, `invalidate_embeddings_cache`.
- Produces (all `pub` in `infigraph_core::embed` except the header helper):
  - `pub fn fnv1a64(bytes: &[u8]) -> u64` — never returns 0 (0 is reserved for "unknown"; a genuine 0 maps to 1)
  - `pub const EMBEDDINGS_FORMAT_VERSION_HASHED: u8 = 3`
  - `fn embeddings_header(header: &[u8]) -> Result<(usize, u8)>` — replaces `embeddings_count_offset`; returns `(count_offset, version)`, legacy = `(0, 0)`
  - `pub fn save_embeddings_hashed(path: &Path, entries: &[(String, Vec<f32>, u64)]) -> Result<()>` — writes v3
  - `pub fn load_embeddings_hashed(path: &Path) -> Result<Vec<(String, Vec<f32>, u64)>>` — reads legacy/v2 (hash 0) and v3
  - v3 entry layout: `id_len u32 | id | input_hash u64 | dim u32 | floats`; header/count/checksum positions identical to v2.

- [ ] **Step 1: Write the failing tests**

Create `crates/infigraph-core/tests/embed_hash_format.rs`:

```rust
use infigraph_core::embed::{
    fnv1a64, load_embeddings, load_embeddings_hashed, save_embeddings, save_embeddings_hashed,
};

fn entries() -> Vec<(String, Vec<f32>, u64)> {
    vec![
        ("a.py::foo".to_string(), vec![0.1f32, 0.2, 0.3], fnv1a64(b"foo-text")),
        ("a.py::bar".to_string(), vec![0.4, 0.5, 0.6], 0), // unknown hash is representable
    ]
}

#[test]
fn fnv1a64_is_stable_and_never_zero() {
    assert_eq!(fnv1a64(b"hello"), fnv1a64(b"hello"));
    assert_ne!(fnv1a64(b"hello"), fnv1a64(b"hellp"));
    assert_ne!(fnv1a64(b""), 0, "0 is reserved for 'unknown'");
}

#[test]
fn v3_roundtrip_preserves_ids_vectors_and_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("embeddings.bin");
    save_embeddings_hashed(&path, &entries()).unwrap();

    let data = std::fs::read(&path).unwrap();
    assert_eq!(&data[0..4], b"IGE1");
    assert_eq!(data[4], 3, "hashed save must write format version 3");

    let loaded = load_embeddings_hashed(&path).unwrap();
    assert_eq!(loaded, entries());

    // The plain loader reads v3 too, stripping hashes.
    let plain = load_embeddings(&path).unwrap();
    assert_eq!(plain.len(), 2);
    assert_eq!(plain[0].0, "a.py::foo");
    assert!((plain[0].1[1] - 0.2).abs() < 1e-6);
}

#[test]
fn v2_file_loads_with_unknown_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("embeddings.bin");
    let pairs = vec![("a.py::foo".to_string(), vec![0.1f32, 0.2])];
    save_embeddings(&path, &pairs).unwrap(); // still writes v2

    let data = std::fs::read(&path).unwrap();
    assert_eq!(data[4], 2, "plain save must keep writing v2");

    let loaded = load_embeddings_hashed(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].2, 0, "v2 entries carry 'unknown' hash");
}

#[test]
fn v3_checksum_detects_corruption() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("embeddings.bin");
    save_embeddings_hashed(&path, &entries()).unwrap();
    let mut data = std::fs::read(&path).unwrap();
    let mid = data.len() / 2;
    data[mid] ^= 0xFF;
    std::fs::write(&path, &data).unwrap();
    assert!(load_embeddings_hashed(&path).is_err());
    assert!(load_embeddings(&path).is_err());
}

#[test]
fn unknown_version_still_rejected_by_both_loaders() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("embeddings.bin");
    let mut data = b"IGE1".to_vec();
    data.push(9); // unknown version
    data.extend_from_slice(&1u32.to_le_bytes());
    std::fs::write(&path, &data).unwrap();
    assert!(load_embeddings(&path).is_err());
    assert!(load_embeddings_hashed(&path).is_err());
}

#[test]
fn embedding_count_reads_v3() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();
    let path = tmp.path().join(".infigraph").join("embeddings.bin");
    save_embeddings_hashed(&path, &entries()).unwrap();
    assert_eq!(infigraph_core::embed::embedding_count(tmp.path()), 2);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test embed_hash_format`
Expected: FAIL to compile — `fnv1a64`, `save_embeddings_hashed`, `load_embeddings_hashed` not found.

- [ ] **Step 3: Implement**

In `crates/infigraph-core/src/embed/mod.rs`:

(a) Constants (next to `EMBEDDINGS_FORMAT_VERSION` at L448):

```rust
/// v3 = v2 plus a per-entry FNV-1a input-text hash for embed-skip.
pub const EMBEDDINGS_FORMAT_VERSION_HASHED: u8 = 3;
```

(b) `fnv1a64` (near `rich_symbol_text_full`):

```rust
/// FNV-1a 64-bit. Used as the embedding-input fingerprint in v3 files.
/// 0 is reserved to mean "unknown", so a genuine hash of 0 maps to 1
/// (worst case: that one symbol always re-embeds).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if h == 0 { 1 } else { h }
}
```

(c) Replace `embeddings_count_offset` (L462-473) with the version-returning form, and update its two callers (`embedding_count` L408-445 and `load_embeddings` L529-597) to destructure the tuple:

```rust
/// Single source of truth for the embeddings.bin header layout: given the
/// leading bytes (at least 5, when available), returns (count_offset, version).
/// Legacy headerless files report version 0.
fn embeddings_header(header: &[u8]) -> Result<(usize, u8)> {
    if header.len() >= 5 && header[0..4] == EMBEDDINGS_MAGIC {
        let version = header[4];
        anyhow::ensure!(
            version == EMBEDDINGS_FORMAT_VERSION || version == EMBEDDINGS_FORMAT_VERSION_HASHED,
            "unsupported embeddings format version: {version}"
        );
        Ok((5, version))
    } else {
        Ok((0, 0))
    }
}
```

(d) `save_embeddings_hashed`: copy `save_embeddings`'s body (keeping the checksum NOTE discipline: every chunk written also goes through `hasher.write`), with two differences — write `EMBEDDINGS_FORMAT_VERSION_HASHED` as the version byte, and after the id bytes write the hash:

```rust
            let hash_bytes = h.to_le_bytes();
            w.write_all(&hash_bytes)?;
            hasher.write(&hash_bytes);
```

(e) Refactor the entry-parsing loop of `load_embeddings` into the shared v-aware core, and make both loaders delegate to it:

```rust
pub fn load_embeddings_hashed(path: &Path) -> Result<Vec<(String, Vec<f32>, u64)>> {
    load_embeddings_impl(path)
}

pub fn load_embeddings(path: &Path) -> Result<Vec<(String, Vec<f32>)>> {
    Ok(load_embeddings_impl(path)?
        .into_iter()
        .map(|(id, v, _)| (id, v))
        .collect())
}
```

`load_embeddings_impl` is the current `load_embeddings` body with the version from `embeddings_header` in scope; inside the per-entry loop, after reading the id:

```rust
        let input_hash = if version == EMBEDDINGS_FORMAT_VERSION_HASHED {
            anyhow::ensure!(pos + 8 <= payload.len(), "truncated embeddings file");
            let h = u64::from_le_bytes(payload[pos..pos + 8].try_into().unwrap());
            pos += 8;
            h
        } else {
            0
        };
```

and push `(id, vec, input_hash)`. The checksum verification is version-independent (it covers the payload bytes as written) — leave it as is.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test embed_hash_format && cargo test -p infigraph-core --test health_signals && cargo test -p infigraph-core --test embed_atomicity`
Expected: all pass — the existing health/atomicity contracts must survive untouched.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/embed/mod.rs crates/infigraph-core/tests/embed_hash_format.rs
git commit -m "feat(embed): embeddings.bin v3 carries per-symbol input-text hashes"
```

---

### Task 2: Early-cutoff in `update_embeddings` + save/HNSW skip

**Files:**
- Modify: `crates/infigraph-core/src/embed/mod.rs:603-685` (`update_embeddings`)
- Test: Create `crates/infigraph-core/tests/embed_skip.rs`

**Interfaces:**
- Consumes: `fnv1a64`, `save_embeddings_hashed`, `load_embeddings_hashed` from Task 1; existing `rich_symbol_text_full(kind, name, file, lang, doc, params, ret)`, `best_embedder`, `build_hnsw_index`, `HNSW_THRESHOLD`.
- Produces: no signature change. New behavior: hash-match symbols keep their stored vector; if nothing was embedded, added, or pruned, the function returns without touching `embeddings.bin` or the HNSW index.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/tests/embed_skip.rs`. Mirror `index_perf.rs`'s fixture idiom (`crates/infigraph-core/tests/index_perf.rs:153-167`): copy its `python_registry()` helper and the `Infigraph::open(root, registry)` + `ig.init()` setup, then drive everything through the public indexing API.

```rust
// [python_registry() helper copied from index_perf.rs goes here]

use infigraph_core::Infigraph;

const FILE_A: &str = "src/alpha.py";

fn write_alpha(root: &std::path::Path, body_line: &str, docstring: &str) {
    let src = format!(
        "def helper(x):\n    \"\"\"{docstring}\"\"\"\n    {body_line}\n    return x\n\ndef other():\n    \"\"\"Stable other function.\"\"\"\n    return 1\n"
    );
    std::fs::write(root.join(FILE_A), src).unwrap();
}

fn setup() -> (tempfile::TempDir, Infigraph) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    write_alpha(dir.path(), "x = 1", "Helper doc.");
    let mut ig = Infigraph::open(dir.path(), python_registry()).unwrap();
    ig.init().unwrap();
    ig.index().unwrap();
    (dir, ig)
}

fn emb_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".infigraph").join("embeddings.bin")
}

#[test]
fn full_index_writes_v3_with_real_hashes() {
    let (dir, _ig) = setup();
    let data = std::fs::read(emb_path(dir.path())).unwrap();
    assert_eq!(data[4], 3, "indexing should write hashed v3 format");
    let entries = infigraph_core::embed::load_embeddings_hashed(&emb_path(dir.path())).unwrap();
    assert!(!entries.is_empty());
    assert!(entries.iter().all(|(_, _, h)| *h != 0), "all hashes should be known");
}

#[test]
fn body_only_edit_skips_rewrite_entirely() {
    let (dir, mut ig) = setup();
    let before = std::fs::read(emb_path(dir.path())).unwrap();

    write_alpha(dir.path(), "x = 2", "Helper doc."); // body change only
    ig.index().unwrap();

    let after = std::fs::read(emb_path(dir.path())).unwrap();
    assert_eq!(before, after, "no input changed → embeddings.bin must not be rewritten");
}

#[test]
fn docstring_edit_reembeds_only_that_symbol() {
    let (dir, mut ig) = setup();
    let before = infigraph_core::embed::load_embeddings_hashed(&emb_path(dir.path())).unwrap();

    write_alpha(dir.path(), "x = 1", "Helper doc CHANGED."); // docstring change
    ig.index().unwrap();

    let after = infigraph_core::embed::load_embeddings_hashed(&emb_path(dir.path())).unwrap();
    let get = |set: &[(String, Vec<f32>, u64)], name: &str| {
        set.iter().find(|(id, _, _)| id.ends_with(name)).cloned().unwrap()
    };
    let (_, _, h_helper_before) = get(&before, "::helper");
    let (_, v_other_before, h_other_before) = get(&before, "::other");
    let (_, _, h_helper_after) = get(&after, "::helper");
    let (_, v_other_after, h_other_after) = get(&after, "::other");

    assert_ne!(h_helper_before, h_helper_after, "changed docstring → new input hash");
    assert_eq!(h_other_before, h_other_after, "untouched symbol keeps its hash");
    assert_eq!(v_other_before, v_other_after, "untouched symbol keeps its exact vector");
}

#[test]
fn deleted_file_still_prunes_embeddings() {
    let (dir, mut ig) = setup();
    std::fs::remove_file(dir.path().join(FILE_A)).unwrap();
    ig.index().unwrap();
    let entries = infigraph_core::embed::load_embeddings_hashed(&emb_path(dir.path())).unwrap();
    assert!(
        !entries.iter().any(|(id, _, _)| id.contains("alpha.py")),
        "symbols from the deleted file must be pruned"
    );
}
```

If `ig.index()` is not the exact method name on `Infigraph`, use whatever `index_perf.rs`'s tests call to run a full/incremental index — mirror that file exactly; do not invent an API.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test embed_skip`
Expected: `full_index_writes_v3_with_real_hashes` FAILS (`data[4]` is 2 — nothing writes v3 yet) and `body_only_edit_skips_rewrite_entirely` FAILS (file is rewritten today).

- [ ] **Step 3: Implement the new `update_embeddings` body**

Replace L603-685 with:

```rust
pub fn update_embeddings(
    backend: &dyn crate::graph::GraphBackend,
    root: &Path,
    changed_files: &[&str],
) -> Result<usize> {
    use rayon::prelude::*;
    use std::sync::Arc;

    let rows = backend.raw_query("MATCH (s:Symbol) RETURN s.id, s.name, s.kind, s.file, s.docstring, s.language, s.parameters, s.return_type")?;
    if rows.is_empty() {
        return Ok(0);
    }

    let emb_path = root.join(".infigraph").join("embeddings.bin");
    // id -> (vector, input_hash); hash 0 = unknown (legacy/v2 files).
    let mut existing: std::collections::HashMap<String, (Vec<f32>, u64)> =
        load_embeddings_hashed(&emb_path)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, v, h)| (id, (v, h)))
            .collect();

    let changed_set: std::collections::HashSet<&str> = changed_files.iter().copied().collect();

    // Early cutoff: a symbol in scope only re-embeds when its input text hash
    // differs from the one stored beside its vector.
    let mut to_embed: Vec<(String, String, u64)> = Vec::new();
    for row in &rows {
        let id = &row[0];
        let file = row.get(3).map(|s| s.as_str()).unwrap_or("");
        let in_scope = changed_set.is_empty()
            || changed_set.contains(file)
            || !existing.contains_key(id.as_str());
        if !in_scope {
            continue;
        }
        let name = &row[1];
        let kind = &row[2];
        let doc = row.get(4).map(|s| s.as_str()).unwrap_or("");
        let lang = row.get(5).map(|s| s.as_str()).unwrap_or("");
        let params = row.get(6).map(|s| s.as_str()).unwrap_or("");
        let ret = row.get(7).map(|s| s.as_str()).unwrap_or("");
        let text = rich_symbol_text_full(kind, name, file, lang, doc, params, ret);
        let h = fnv1a64(text.as_bytes());
        match existing.get(id.as_str()) {
            Some((_, stored)) if *stored == h => {} // input unchanged: keep the vector
            _ => to_embed.push((id.clone(), text, h)),
        }
    }

    let mut embedded = 0usize;
    if !to_embed.is_empty() {
        let embedder: Arc<Box<dyn EmbedProvider>> = Arc::new(best_embedder());
        const BATCH: usize = 256;
        let results: Vec<Vec<(String, Vec<f32>, u64)>> = to_embed
            .par_chunks(BATCH)
            .map(|chunk| {
                let emb = Arc::clone(&embedder);
                let texts: Vec<&str> = chunk.iter().map(|(_, t, _)| t.as_str()).collect();
                let vecs = emb.embed_batch(&texts).unwrap_or_default();
                chunk
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (id, _, h))| vecs.get(i).map(|v| (id.clone(), v.clone(), *h)))
                    .collect()
            })
            .collect();
        for batch in results {
            for (id, v, h) in batch {
                existing.insert(id, (v, h));
                embedded += 1;
            }
        }
    }

    let all_ids: std::collections::HashSet<String> = rows.iter().map(|r| r[0].clone()).collect();
    let before_prune = existing.len();
    existing.retain(|id, _| all_ids.contains(id));
    let pruned = before_prune - existing.len();

    let count = existing.len();

    // Nothing embedded, nothing pruned: the file's contents would be identical.
    // Skip the write AND the O(n) HNSW rebuild — this is the body-only-edit
    // fast path.
    if embedded == 0 && pruned == 0 {
        return Ok(count);
    }

    let entries: Vec<(String, Vec<f32>, u64)> = existing
        .into_iter()
        .map(|(id, (v, h))| (id, v, h))
        .collect();
    save_embeddings_hashed(&emb_path, &entries)?;

    let symbol_embeddings: Vec<(String, Vec<f32>)> = entries
        .iter()
        .map(|(id, v, _)| (id.clone(), v.clone()))
        .collect();

    // Build/rebuild HNSW only when above threshold OR when an existing index
    // needs to stay current after incremental updates.
    let hnsw_path = root.join(".infigraph").join("hnsw_index.usearch");
    let should_build = count >= HNSW_THRESHOLD || hnsw_path.exists();
    if should_build {
        invalidate_hnsw_cache();
        if let Err(e) = build_hnsw_index(&symbol_embeddings, &hnsw_path, &emb_path) {
            eprintln!("warning: HNSW index build failed ({e}), vector search will use brute-force");
        }
    }

    Ok(count)
}
```

Note the one intentional cost: `symbol_embeddings` clones ids+vectors for `build_hnsw_index` (~1.5 KB/symbol; ~15 MB at 10k symbols) — same order of copying the old code did via `existing.into_iter().collect()`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test embed_skip && cargo test -p infigraph-core --test embed_hash_format && cargo test -p infigraph-core --test search_perf`
Expected: all pass (`search_perf` exercises `save_embeddings`/`load_embeddings` round-trips at scale — proves the plain v2 path still works).

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/embed/mod.rs crates/infigraph-core/tests/embed_skip.rs
git commit -m "perf(embed): skip re-embedding and HNSW rebuild when symbol inputs are unchanged"
```

---

### Task 3: Sweep, docs, and verification

**Files:**
- Modify: `docs/CODE-PARSING.md` (Embeddings section), `ARCHITECTURE.md` (Incremental Indexing section)

- [ ] **Step 1: Update docs**

In `docs/CODE-PARSING.md`'s Embeddings section and `ARCHITECTURE.md`'s "8. Incremental Indexing", add 2-3 sentences each: embeddings.bin v3 stores a per-symbol FNV-1a input hash; `update_embeddings` re-embeds only on hash mismatch; when nothing changed the save and HNSW rebuild are skipped entirely; v2 files upgrade to v3 on their first post-change save.

- [ ] **Step 2: Per-crate then full verification** (disk-constrained ordering)

```bash
cargo fmt --all -- --check
cargo clippy -p infigraph-core --all-targets -- -D warnings
cargo test -p infigraph-core
env -u INFIGRAPH_WATCH_DAEMON cargo test --all
```
Expected: all green. If `--all` shows resource-contention flakes, confirm with `--test-threads=1` before treating anything as a regression (repo CLAUDE.md).

- [ ] **Step 3: Commit**

```bash
git add docs/CODE-PARSING.md ARCHITECTURE.md
git commit -m "docs: describe embeddings.bin v3 input-hash skip"
```
