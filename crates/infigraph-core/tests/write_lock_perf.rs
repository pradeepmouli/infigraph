use std::sync::Arc;
use std::time::{Duration, Instant};

use infigraph_core::graph::GraphStore;
use infigraph_core::model::{FileExtraction, Span, Symbol, SymbolKind};
use tempfile::TempDir;

fn make_store() -> (TempDir, GraphStore) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let store = GraphStore::open(&db_path).unwrap();
    (dir, store)
}

fn make_extraction(file: &str) -> FileExtraction {
    FileExtraction {
        file: file.to_string(),
        language: "python".to_string(),
        content_hash: format!("hash_{file}"),
        symbols: vec![Symbol {
            scip_id: None,
            id: format!("{file}::func"),
            name: "func".to_string(),
            kind: SymbolKind::Function,
            span: Span {
                file: file.to_string(),
                start_line: 1,
                start_col: 0,
                end_line: 3,
                end_col: 0,
            },
            signature_hash: "sig1".to_string(),
            parent: None,
            language: "python".to_string(),
            visibility: None,
            docstring: None,
            complexity: 1,
            parameters: None,
            return_type: None,
        }],
        relations: vec![],
        statements: vec![],
    }
}

#[test]
#[ignore] // perf test — run via pre-commit hook, not CI
fn test_lock_overhead_under_1ms() {
    let (_dir, store) = make_store();

    // Warm up: the very first locks pay one-time init/page-fault costs that
    // aren't representative of steady-state overhead.
    for _ in 0..100 {
        drop(store.write_lock().unwrap());
    }

    let start = Instant::now();
    let iterations = 1000;
    for _ in 0..iterations {
        let lock = store.write_lock().unwrap();
        drop(lock);
    }
    let elapsed = start.elapsed();
    let avg = elapsed / iterations;
    // An in-memory advisory lock/unlock is sub-microsecond; this guards against
    // a *gross* regression (e.g. accidentally adding disk I/O or a blocking
    // syscall to the lock path), not micro-jitter. The threshold is deliberately
    // loose (~1000x the real cost) so a loaded dev machine's scheduler jitter
    // doesn't flake it while a genuine 100x+ regression still trips it. The old
    // <1ms wall flaked constantly under machine load (AIF3X-331 #25).
    assert!(
        avg < Duration::from_millis(5),
        "avg lock/unlock should be well under 5ms (regression guard), got {:?}",
        avg
    );
}

/// Upper bound on the contended/single-thread wall-clock ratio the *median*
/// sample must stay under. Overridable via `INFIGRAPH_TEST_LOCK_CONTENTION_RATIO`
/// for a machine known to be loaded (#142); the default stays tight, since a
/// real regression (a clap `Command` built inside the held lock region)
/// tripped the old single-sample gate 5/5 on the day #142 was filed and must
/// keep tripping this one.
fn contended_ratio_bound() -> f64 {
    std::env::var("INFIGRAPH_TEST_LOCK_CONTENTION_RATIO")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8.0)
}

/// One sample: 100 uncontended lock/unlock cycles, then 4 threads doing 100
/// each. Returns `(single_thread, four_threads)` elapsed.
fn contended_throughput_sample(store: &Arc<GraphStore>) -> (Duration, Duration) {
    let start = Instant::now();
    for _ in 0..100 {
        let lock = store.write_lock().unwrap();
        std::hint::black_box(&lock);
        drop(lock);
    }
    let single_thread = start.elapsed();

    let start = Instant::now();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let s = Arc::clone(store);
            std::thread::spawn(move || {
                for _ in 0..100 {
                    let lock = s.write_lock().unwrap();
                    std::hint::black_box(&lock);
                    drop(lock);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    (single_thread, start.elapsed())
}

#[test]
#[ignore] // perf test — run via pre-commit hook, not CI
fn test_contended_lock_throughput() {
    let (_dir, store) = make_store();
    let store = Arc::new(store);

    // Warm up (same reason as test_lock_overhead_under_1ms): first-touch
    // costs would otherwise land in whichever sample runs first.
    for _ in 0..100 {
        drop(store.write_lock().unwrap());
    }

    // Median of 5 samples, not one: a single 100-iteration sample on a busy
    // dev machine (load average 6-9) landed anywhere from 3x to 11x with no
    // code change and failed 2 of 3 commits (#142). The median discards the
    // scheduler-jitter outliers without loosening the bound itself -- 4
    // threads doing 4x the work should still finish well under 8x.
    const SAMPLES: usize = 5;
    let samples: Vec<(Duration, Duration)> = (0..SAMPLES)
        .map(|_| contended_throughput_sample(&store))
        .collect();
    let mut ratios: Vec<f64> = samples
        .iter()
        .map(|(single, multi)| multi.as_secs_f64() / single.as_secs_f64().max(f64::EPSILON))
        .collect();
    ratios.sort_by(|a, b| a.total_cmp(b));
    let median = ratios[SAMPLES / 2];
    let bound = contended_ratio_bound();
    eprintln!(
        "contended lock throughput: median 4-thread/single ratio {median:.2}x (bound {bound}x), \
         samples (single, 4-thread): {samples:?}"
    );

    assert!(
        median < bound,
        "contended throughput too slow: median 4-thread/single ratio {median:.2}x is not under \
         {bound}x across {SAMPLES} samples {samples:?}. A median far beyond the bound is a real \
         regression in the lock path; on a machine known to be heavily loaded, widen it with \
         INFIGRAPH_TEST_LOCK_CONTENTION_RATIO (see pradeepmouli/infigraph#142)."
    );
}

#[test]
#[ignore] // perf test — run via pre-commit hook, not CI
fn test_no_perf_regression_upsert_file() {
    let (_dir, store) = make_store();

    let warmup = make_extraction("warmup.py");
    store.upsert_file(&warmup).unwrap();

    let mut times = Vec::new();
    for i in 0..20 {
        let ext = make_extraction(&format!("perf{i}.py"));
        let start = Instant::now();
        store.upsert_file(&ext).unwrap();
        times.push(start.elapsed());
    }

    let avg = times.iter().sum::<Duration>() / times.len() as u32;
    // Lock overhead is <1ms; upsert is typically 5-50ms.
    // If avg > 200ms something is very wrong.
    assert!(
        avg < Duration::from_millis(200),
        "avg upsert_file too slow: {:?} (lock overhead should be negligible)",
        avg
    );
}
