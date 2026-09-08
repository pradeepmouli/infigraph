# Design: Persistent doc BM25 cache + symbol-level embedding skip

**Date:** 2026-08-07
**Status:** Approved (brainstorming session)
**Scope:** Two independent search-performance improvements. Pure performance — zero change to search results or graph behavior.

## Motivation

1. `hybrid_doc_search` (`crates/infigraph-docs/src/search.rs:97`) rebuilds its entire
   `DocBM25Index` in memory on every query, while code search already persists its BM25
   index to `.infigraph/bm25_cache.bin` across sessions.
2. `update_embeddings` (`crates/infigraph-core/src/embed/mod.rs:603`) re-embeds **every**
   symbol in a changed file, even though the embedding input
   (`rich_symbol_text_full`: kind/name/file/lang/docstring/params/return-type) excludes
   the function body. Body-only edits — the most common watcher event — re-embed vectors
   that come out byte-identical, then pay a full O(n) HNSW rebuild on top.

Call-edge tracking is unaffected by #2: `CALLS` edges come from tree-sitter extraction +
cross-file resolution and are rebuilt on every file change as today. Only the semantic
ranking layer skips work.

## Improvement 1: Persistent doc BM25 cache

**Branch:** `perf/doc-bm25-cache` off fresh `upstream/main` (843eeb4). Upstream PR
candidate (prepared only — not opened without explicit approval), then merged locally
into `feat/hardening`.

`crates/infigraph-docs/src/search.rs` is byte-identical between `upstream/main` and
`feat/hardening`, so this decouples cleanly from the hardening branch.

### Changes

- `DocBM25Index` gains `save(path)` / `load(path)`.
- Cache file: `docs_bm25_cache.bin` in the same `.infigraph/` directory as the doc store
  it indexes. Keying off the store directory means project stores and group/combined
  stores each get their own cache with no extra code.
- Format: leading version byte + hand-rolled length-prefixed binary, mirroring the
  exact idiom of code search's `BM25Index::save/load`
  (`crates/infigraph-core/src/search/mod.rs:109-183`): `avg_doc_len`, doc count,
  `[id_len, id, text_len, text]*`, term count, `[term_len, term, postings]*`.
  No new dependencies (`bincode` is not in the docs crate). Unlike the code-side
  loader, parsing is bounds-checked so a truncated cache returns `Err` instead of
  panicking (a corrupt cache must degrade to a rebuild, never crash a search).
- Freshness: cache is fresh iff its mtime >= `docs_embeddings.bin`'s mtime — the same
  anchor pattern code search uses (see `bm25_cache_stale_when_embeddings_newer` /
  `bm25_cache_fresh_when_older_than_embeddings` in `crates/infigraph-cli/src/index.rs`).
  Doc reindex always rewrites `docs_embeddings.bin`, so invalidation is automatic.
- `hybrid_doc_search` flow: fresh cache → load and use; missing/stale/unreadable →
  rebuild in memory exactly as today, then save via inline temp+rename in the same
  directory (no dependency on the hardening branch's shared atomic-write helper; after
  the local merge into `feat/hardening`, a follow-up commit there may swap to the shared
  helper).
- A failed save degrades to today's per-query rebuild; it never fails the search.

## Improvement 2: Symbol-level embedding skip

**Branch:** `feat/hardening`. Depends on the embeddings.bin magic+version+checksum
header machinery (`EMBEDDINGS_FORMAT_VERSION`, `embeddings_count_offset`) which exists
only on the hardening branch — goes upstream later, riding the header hardening.

### Changes

- Add `EMBEDDINGS_FORMAT_VERSION_HASHED = 3` alongside the current version 2. Each v3
  entry gains `input_hash: u64` — FNV-1a of the exact `rich_symbol_text_full(...)`
  string that produced the vector (0 reserved as "unknown"; a genuine hash of 0 maps
  to 1).
- **`save_embeddings`'s public signature is unchanged.** It has 39 references across
  four crates (doc embeddings, session embeddings, combined stores, tests all share
  the format) — hashless writers keep emitting v2 exactly as today. Only
  `update_embeddings` writes v3, via a new `save_embeddings_hashed`. The loader
  accepts legacy/v2/v3; v2 entries load with `hash = 0` (unknown).
- Version dispatch stays inside the single-source-of-truth header helper, extended to
  return `(count_offset, version)`.
- `update_embeddings` skip logic: for each symbol in a changed file, compute the input
  text (already computed today), hash it, and re-embed only if no existing vector exists
  or the stored hash differs. `None` counts as differing, so a pre-upgrade file re-embeds
  changed files once (today's behavior) and converges to the new format on first save.
- Embedding-input semantics are unchanged (decision: keep body text excluded; body
  content remains covered by BM25 and grep).
- HNSW/save skip: if zero symbols were re-embedded, none added, and none pruned by the
  `retain`, skip `save_embeddings` and `build_hnsw_index` entirely. This converts
  body-only edits' embedding phase into a pure hash check.
- `embedding_count` and the `health_signals` version-byte tests are updated for the new
  version. Doctor needs no change: an old binary seeing the new format reports it
  unreadable-regenerable (true), and `infigraph index` rebuilds it.

## Error handling (both)

Both artifacts are regenerable caches. Any read failure — corrupt, truncated, wrong
version — silently falls back to a rebuild; never fatal, never triggers destructive
recovery of anything else. All writes are temp+rename within the same directory
(atomic on the same filesystem). This respects the repo invariant that destructive
recovery is reserved for actual corruption of non-regenerable state.

## Testing (TDD, targeted per crate)

**#1 (`infigraph-docs`):**
- save/load roundtrip preserves scores.
- Stale-when-embeddings-newer and fresh-when-older (mirroring the two cli tests).
- Corrupt/truncated cache → silent rebuild, search still succeeds.
- Search results identical with and without cache.

**#2 (`infigraph-core`):**
- Counting test `EmbedProvider`: body-only change → 0 embed calls; docstring change →
  exactly 1 embed call for that symbol.
- Legacy-format file → changed file re-embeds once, saves in new format.
- Deleted symbols still pruned from embeddings.
- No changes at all → `save_embeddings` and HNSW rebuild both skipped.
- Version-byte rejection tests updated (unknown versions still rejected).

## Success criteria

- Doc search: no `DocBM25Index` rebuild on repeat queries against an unchanged store.
- Watcher body-only edit: zero embed-model invocations, zero HNSW rebuild.
- `cargo test` green per touched crate, then full suite; fmt/clippy clean.
