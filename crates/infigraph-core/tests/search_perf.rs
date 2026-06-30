use std::time::Instant;

use infigraph_core::embed::{self, EmbedProvider};
use infigraph_core::search::{self, BM25Index};

fn generate_symbol_docs(count: usize) -> Vec<(String, String)> {
    let kinds = ["function", "class", "method", "variable", "module"];
    let words = [
        "authenticate",
        "validate",
        "process",
        "serialize",
        "transform",
        "calculate",
        "dispatch",
        "render",
        "initialize",
        "configure",
        "payment",
        "session",
        "request",
        "handler",
        "middleware",
        "database",
        "cache",
        "queue",
        "worker",
        "scheduler",
    ];
    (0..count)
        .map(|i| {
            let kind = kinds[i % kinds.len()];
            let w1 = words[i % words.len()];
            let w2 = words[(i * 7 + 3) % words.len()];
            let id = format!("sym::{}_{}", w1, i);
            let text = format!(
                "{} {}_{}: {} {} operation for module {}",
                kind,
                w1,
                i,
                w2,
                w1,
                i / 10
            );
            (id, text)
        })
        .collect()
}

fn generate_embeddings(
    docs: &[(String, String)],
    embedder: &dyn EmbedProvider,
) -> Vec<(String, Vec<f32>)> {
    docs.iter()
        .map(|(id, text)| {
            let emb = embedder
                .embed(text)
                .unwrap_or_else(|_| vec![0.0; embedder.dimension()]);
            (id.clone(), emb)
        })
        .collect()
}

const BENCHMARK_QUERIES: &[&str] = &[
    "authenticate user login",
    "process payment transaction",
    "validate email address",
    "database connection pool",
    "serialize json response",
    "handle http request",
    "render component template",
    "configure middleware",
    "dispatch event handler",
    "cache invalidation strategy",
];

// ---------------------------------------------------------------------------
// Synthetic benchmarks (run in CI with #[ignore])
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn bench_bm25_build_10k() {
    let docs = generate_symbol_docs(10_000);
    let start = Instant::now();
    let index = BM25Index::build(docs);
    let elapsed = start.elapsed();
    eprintln!("[perf] BM25 build 10K symbols: {:?}", elapsed);

    let start = Instant::now();
    for q in BENCHMARK_QUERIES {
        let _ = index.search(q, 20);
    }
    let query_elapsed = start.elapsed();
    eprintln!(
        "[perf] BM25 10 queries on 10K: {:?} ({:?}/query)",
        query_elapsed,
        query_elapsed / 10
    );
}

#[test]
#[ignore]
fn bench_bm25_build_100k() {
    let docs = generate_symbol_docs(100_000);
    let start = Instant::now();
    let index = BM25Index::build(docs);
    let elapsed = start.elapsed();
    eprintln!("[perf] BM25 build 100K symbols: {:?}", elapsed);

    let start = Instant::now();
    for q in BENCHMARK_QUERIES {
        let _ = index.search(q, 20);
    }
    let query_elapsed = start.elapsed();
    eprintln!(
        "[perf] BM25 10 queries on 100K: {:?} ({:?}/query)",
        query_elapsed,
        query_elapsed / 10
    );
}

#[test]
#[ignore]
fn bench_embedder_creation() {
    let start = Instant::now();
    let embedder = embed::best_embedder();
    let elapsed = start.elapsed();
    eprintln!("[perf] best_embedder() creation: {:?}", elapsed);

    let start = Instant::now();
    let _ = embedder.embed("authenticate user login session");
    let single_elapsed = start.elapsed();
    eprintln!("[perf] single embed(): {:?}", single_elapsed);

    let texts: Vec<&str> = BENCHMARK_QUERIES.to_vec();
    let start = Instant::now();
    let _ = embedder.embed_batch(&texts);
    let batch_elapsed = start.elapsed();
    eprintln!(
        "[perf] embed_batch({} queries): {:?} ({:?}/query)",
        texts.len(),
        batch_elapsed,
        batch_elapsed / texts.len() as u32
    );
}

#[test]
#[ignore]
fn bench_embeddings_save_load() {
    let docs = generate_symbol_docs(10_000);
    let embedder = embed::best_embedder();
    let embeddings = generate_embeddings(&docs, embedder.as_ref());

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("embeddings.bin");

    let start = Instant::now();
    embed::save_embeddings(&path, &embeddings).unwrap();
    let save_elapsed = start.elapsed();
    let file_size = std::fs::metadata(&path).unwrap().len();
    eprintln!(
        "[perf] save 10K embeddings: {:?} ({} bytes, {:.1} MB)",
        save_elapsed,
        file_size,
        file_size as f64 / 1_048_576.0
    );

    let start = Instant::now();
    let loaded = embed::load_embeddings_cached(&path).unwrap();
    let cold_elapsed = start.elapsed();
    eprintln!("[perf] load 10K embeddings (cold): {:?}", cold_elapsed);
    assert_eq!(loaded.len(), embeddings.len());

    let start = Instant::now();
    let _loaded2 = embed::load_embeddings_cached(&path).unwrap();
    let warm_elapsed = start.elapsed();
    eprintln!(
        "[perf] load 10K embeddings (warm/cached): {:?}",
        warm_elapsed
    );
}

#[test]
#[ignore]
fn bench_hybrid_search_10k() {
    let docs = generate_symbol_docs(10_000);
    let embedder = embed::best_embedder();
    let embeddings = generate_embeddings(&docs, embedder.as_ref());
    let bm25_index = BM25Index::build(docs.clone());

    let start = Instant::now();
    for q in BENCHMARK_QUERIES {
        let _ = search::hybrid_search(
            q,
            &bm25_index,
            embedder.as_ref(),
            &embeddings,
            20,
            0.3,
            None,
            None,
        )
        .unwrap();
    }
    let elapsed = start.elapsed();
    eprintln!(
        "[perf] hybrid_search 10 queries on 10K (no HNSW): {:?} ({:?}/query)",
        elapsed,
        elapsed / 10
    );
}

#[test]
#[ignore]
fn bench_hybrid_search_with_hnsw_10k() {
    let docs = generate_symbol_docs(10_000);
    let embedder = embed::best_embedder();
    let embeddings = generate_embeddings(&docs, embedder.as_ref());
    let bm25_index = BM25Index::build(docs.clone());

    let dir = tempfile::TempDir::new().unwrap();
    let emb_path = dir.path().join("embeddings.bin");
    let hnsw_path = dir.path().join("hnsw_index.usearch");
    embed::save_embeddings(&emb_path, &embeddings).unwrap();

    let start = Instant::now();
    embed::build_hnsw_index(&embeddings, &hnsw_path, &emb_path).unwrap();
    let build_elapsed = start.elapsed();
    eprintln!("[perf] HNSW build 10K: {:?}", build_elapsed);

    let start = Instant::now();
    for q in BENCHMARK_QUERIES {
        let _ = search::hybrid_search(
            q,
            &bm25_index,
            embedder.as_ref(),
            &embeddings,
            20,
            0.3,
            Some(&hnsw_path),
            Some(&emb_path),
        )
        .unwrap();
    }
    let elapsed = start.elapsed();
    eprintln!(
        "[perf] hybrid_search 10 queries on 10K (with HNSW): {:?} ({:?}/query)",
        elapsed,
        elapsed / 10
    );
}

// ---------------------------------------------------------------------------
// Golden corpus — captures top-5 IDs for reproducibility validation
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn bench_golden_corpus_stability() {
    let docs = generate_symbol_docs(10_000);
    let bm25_index = BM25Index::build(docs.clone());

    // Pure BM25 golden corpus — fully deterministic
    let mut run1_ids: Vec<Vec<String>> = Vec::new();
    for q in BENCHMARK_QUERIES {
        let results = bm25_index.search(q, 5);
        let ids: Vec<String> = results
            .iter()
            .map(|(idx, _)| bm25_index.doc_id(*idx).to_string())
            .collect();
        run1_ids.push(ids);
    }

    let bm25_index2 = BM25Index::build(docs);
    let mut run2_ids: Vec<Vec<String>> = Vec::new();
    for q in BENCHMARK_QUERIES {
        let results = bm25_index2.search(q, 5);
        let ids: Vec<String> = results
            .iter()
            .map(|(idx, _)| bm25_index2.doc_id(*idx).to_string())
            .collect();
        run2_ids.push(ids);
    }

    for (i, q) in BENCHMARK_QUERIES.iter().enumerate() {
        assert_eq!(
            run1_ids[i], run2_ids[i],
            "golden corpus unstable for query '{}': run1={:?} vs run2={:?}",
            q, run1_ids[i], run2_ids[i]
        );
        eprintln!("[golden] '{}' → {:?}", q, run1_ids[i]);
    }
}

// ---------------------------------------------------------------------------
// Full cold-start simulation (all phases timed individually)
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn bench_full_cold_start_breakdown() {
    let symbol_count = 50_000;
    eprintln!(
        "[perf] === Cold-start breakdown ({} symbols) ===",
        symbol_count
    );

    // Phase: generate docs (simulates Kuzu query)
    let start = Instant::now();
    let docs = generate_symbol_docs(symbol_count);
    let docs_elapsed = start.elapsed();
    eprintln!(
        "[perf] 1. generate {} symbol docs: {:?}",
        symbol_count, docs_elapsed
    );

    // Phase: BM25 build
    let start = Instant::now();
    let bm25_index = BM25Index::build(docs.clone());
    let bm25_elapsed = start.elapsed();
    eprintln!("[perf] 2. BM25 build: {:?}", bm25_elapsed);

    // Phase: embedder creation
    let start = Instant::now();
    let embedder = embed::best_embedder();
    let embedder_elapsed = start.elapsed();
    eprintln!("[perf] 3. best_embedder() init: {:?}", embedder_elapsed);

    // Phase: generate embeddings (simulates load from disk)
    let start = Instant::now();
    let embeddings = generate_embeddings(&docs, embedder.as_ref());
    let emb_elapsed = start.elapsed();
    eprintln!(
        "[perf] 4. embed {} symbols: {:?}",
        symbol_count, emb_elapsed
    );

    // Phase: save + cold load
    let dir = tempfile::TempDir::new().unwrap();
    let emb_path = dir.path().join("embeddings.bin");
    embed::save_embeddings(&emb_path, &embeddings).unwrap();
    let file_size = std::fs::metadata(&emb_path).unwrap().len();
    eprintln!(
        "[perf]    embeddings.bin size: {:.1} MB",
        file_size as f64 / 1_048_576.0
    );

    embed::invalidate_embeddings_cache();
    let start = Instant::now();
    let _loaded = embed::load_embeddings_cached(&emb_path).unwrap();
    let load_elapsed = start.elapsed();
    eprintln!("[perf] 5. load embeddings (cold): {:?}", load_elapsed);

    // Phase: HNSW build + search
    let hnsw_path = dir.path().join("hnsw_index.usearch");
    let start = Instant::now();
    embed::build_hnsw_index(&embeddings, &hnsw_path, &emb_path).unwrap();
    let hnsw_build_elapsed = start.elapsed();
    let hnsw_size = std::fs::metadata(&hnsw_path).unwrap().len();
    let meta_path = hnsw_path.with_extension("meta");
    let meta_size = std::fs::metadata(&meta_path).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "[perf] 6. HNSW build: {:?} (index: {:.1} MB, sidecar: {:.1} MB)",
        hnsw_build_elapsed,
        hnsw_size as f64 / 1_048_576.0,
        meta_size as f64 / 1_048_576.0
    );

    // Phase: query
    let start = Instant::now();
    for q in BENCHMARK_QUERIES {
        let _ = search::hybrid_search(
            q,
            &bm25_index,
            embedder.as_ref(),
            &embeddings,
            20,
            0.3,
            Some(&hnsw_path),
            Some(&emb_path),
        )
        .unwrap();
    }
    let query_elapsed = start.elapsed();
    eprintln!(
        "[perf] 7. 10 hybrid queries: {:?} ({:?}/query)",
        query_elapsed,
        query_elapsed / 10
    );

    let total = docs_elapsed + bm25_elapsed + embedder_elapsed + load_elapsed + query_elapsed;
    eprintln!(
        "[perf] === Total cold-start (excl embedding generation): {:?} ===",
        total
    );
    eprintln!(
        "[perf]    docs={:?} bm25={:?} embedder={:?} load={:?} query={:?}",
        docs_elapsed, bm25_elapsed, embedder_elapsed, load_elapsed, query_elapsed
    );
}
