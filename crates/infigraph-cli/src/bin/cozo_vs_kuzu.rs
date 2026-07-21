use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use infigraph_core::extract::extract_file;
use infigraph_core::graph::{CozoStore, GraphQuery, GraphStore};
use infigraph_languages::bundled_registry;

fn main() -> Result<()> {
    let target_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests/fixtures/python-simple"));

    if !target_dir.exists() {
        anyhow::bail!("dir not found: {}", target_dir.display());
    }

    let registry = bundled_registry()?;

    // Collect all files
    let mut files = Vec::new();
    collect_files(&target_dir, &mut files);
    files.sort();
    eprintln!("Found {} files in {}", files.len(), target_dir.display());

    // Extract all files
    let t0 = Instant::now();
    let mut extractions = Vec::new();
    let mut skipped = 0usize;
    for path in &files {
        let rel_path = path.to_string_lossy().replace('\\', "/");
        let source = match std::fs::read(path) {
            Ok(s) => s,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let pack = match registry.for_file_with_content(&rel_path, &source) {
            Some(p) => p,
            None => {
                skipped += 1;
                continue;
            }
        };
        match extract_file(&rel_path, &source, pack) {
            Ok(e) => extractions.push(e),
            Err(_) => {
                skipped += 1;
            }
        }
    }
    let extract_ms = t0.elapsed().as_millis();
    let total_symbols: usize = extractions.iter().map(|e| e.symbols.len()).sum();
    let total_relations: usize = extractions.iter().map(|e| e.relations.len()).sum();
    let total_stmts: usize = extractions.iter().map(|e| e.statements.len()).sum();
    eprintln!(
        "Extracted {} files ({} skipped) in {}ms: {} symbols, {} relations, {} statements",
        extractions.len(),
        skipped,
        extract_ms,
        total_symbols,
        total_relations,
        total_stmts
    );

    let tmp = std::env::temp_dir().join("cozo_vs_kuzu_bench");
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    std::fs::create_dir_all(&tmp)?;

    let kuzu_path = tmp.join("graph");
    let cozo_path = tmp.join("graph.cozo");

    // === Kuzu indexing ===
    eprintln!("\n=== Kuzu ===");
    let t0 = Instant::now();
    let kuzu = GraphStore::open(&kuzu_path)?;
    let kuzu_open_ms = t0.elapsed().as_millis();

    let t0 = Instant::now();
    let conn = kuzu.connection()?;
    let lock = kuzu.write_lock()?;
    for extraction in &extractions {
        kuzu.upsert_file_conn(&conn, extraction, &lock)?;
    }
    drop(lock);
    let kuzu_write_ms = t0.elapsed().as_millis();

    let t0 = Instant::now();
    let kuzu_stats = kuzu.stats()?;
    let kuzu_stats_ms = t0.elapsed().as_millis();
    eprintln!(
        "open={}ms  write={}ms  stats={}ms",
        kuzu_open_ms, kuzu_write_ms, kuzu_stats_ms
    );
    eprintln!(
        "{} symbols, {} calls, {} modules, {} files",
        kuzu_stats.symbols, kuzu_stats.calls, kuzu_stats.modules, kuzu_stats.files
    );

    // === CozoDB indexing ===
    eprintln!("\n=== CozoDB ===");
    let t0 = Instant::now();
    let cozo = CozoStore::open(&cozo_path)?;
    let cozo_open_ms = t0.elapsed().as_millis();

    let t0 = Instant::now();
    for extraction in &extractions {
        cozo.upsert_file_batch(extraction)?;
    }
    cozo.refresh_materialized()?;
    let cozo_write_ms = t0.elapsed().as_millis();

    let t0 = Instant::now();
    let cozo_counts = cozo.relation_counts()?;
    let cozo_counts_ms = t0.elapsed().as_millis();
    let cozo_stats = cozo.stats()?;
    eprintln!(
        "open={}ms  write={}ms  counts={}ms",
        cozo_open_ms, cozo_write_ms, cozo_counts_ms
    );
    eprintln!(
        "{} symbols, {} calls, {} modules, {} files",
        cozo_stats.symbols,
        cozo_counts.get("calls").copied().unwrap_or(0),
        cozo_stats.modules,
        cozo_stats.files
    );

    // === Write perf summary ===
    eprintln!("\n=== Write Performance ===");
    eprintln!("{:<15} {:>8} {:>8}  {:>8}", "", "Kuzu", "CozoDB", "Speedup");
    eprintln!("{}", "-".repeat(45));
    print_perf("DB open", kuzu_open_ms, cozo_open_ms);
    print_perf("Write all", kuzu_write_ms, cozo_write_ms);
    print_perf("Stats", kuzu_stats_ms, cozo_counts_ms);
    let kuzu_total = kuzu_open_ms + kuzu_write_ms + kuzu_stats_ms;
    let cozo_total = cozo_open_ms + cozo_write_ms + cozo_counts_ms;
    print_perf("TOTAL", kuzu_total, cozo_total);

    // === Count comparison ===
    eprintln!("\n=== Count Verification ===");
    let mut failures = 0u32;
    for (label, kuzu_n, cozo_n) in [
        ("symbols", kuzu_stats.symbols, cozo_stats.symbols),
        ("modules", kuzu_stats.modules, cozo_stats.modules),
        ("files", kuzu_stats.files, cozo_stats.files),
        ("contains", kuzu_stats.contains, cozo_stats.contains),
        ("inherits", kuzu_stats.inherits, cozo_stats.inherits),
    ] {
        let ok = kuzu_n == cozo_n;
        if !ok {
            failures += 1;
        }
        eprintln!(
            "{:<15} Kuzu={:<6} CozoDB={:<6} {}",
            label,
            kuzu_n,
            cozo_n,
            if ok { "✅" } else { "❌" }
        );
    }

    // === Query perf ===
    let kq = GraphQuery::new(&conn);

    // Pick sample file and symbol for queries
    let sample_file = extractions.first().map(|e| e.file.as_str()).unwrap_or("");
    let sample_syms = cozo.symbols_in_file(sample_file)?;
    let sample_id = sample_syms.first().map(|s| s.id.as_str()).unwrap_or("");

    if !sample_id.is_empty() {
        eprintln!(
            "\n=== Query Performance (10 iterations, sample={}) ===",
            sample_id.rsplit("::").next().unwrap_or(sample_id)
        );
        let iters: u128 = 10;

        #[allow(clippy::type_complexity)]
        let queries: Vec<(
            &str,
            Box<dyn Fn() -> Result<()>>,
            Box<dyn Fn() -> Result<()>>,
        )> = vec![
            (
                "symbols_in_file",
                Box::new(|| {
                    kq.symbols_in_file(sample_file)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.symbols_in_file(sample_file)?;
                    Ok(())
                }),
            ),
            (
                "callers_of",
                Box::new(|| {
                    kq.callers_of(sample_id)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.callers_of(sample_id)?;
                    Ok(())
                }),
            ),
            (
                "callees_of",
                Box::new(|| {
                    kq.callees_of(sample_id)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.callees_of(sample_id)?;
                    Ok(())
                }),
            ),
            (
                "find_symbol_by_id",
                Box::new(|| {
                    kq.find_symbol_by_id(sample_id)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.find_symbol_by_id(sample_id)?;
                    Ok(())
                }),
            ),
            (
                "get_api_surface",
                Box::new(|| {
                    kq.get_api_surface()?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.get_api_surface()?;
                    Ok(())
                }),
            ),
            (
                "get_test_coverage",
                Box::new(|| {
                    kq.get_test_coverage()?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.get_test_coverage()?;
                    Ok(())
                }),
            ),
            (
                "transitive_impact",
                Box::new(|| {
                    kq.transitive_impact(sample_id, 3)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.transitive_impact(sample_id, 3)?;
                    Ok(())
                }),
            ),
            (
                "find_all_references",
                Box::new(|| {
                    kq.find_all_references(sample_id)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.find_all_references(sample_id)?;
                    Ok(())
                }),
            ),
            (
                "get_file_deps",
                Box::new(|| {
                    kq.get_file_deps(sample_file)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.get_file_deps(sample_file)?;
                    Ok(())
                }),
            ),
            (
                "symbols_in_range",
                Box::new(|| {
                    kq.symbols_in_range(sample_file, 1, 50)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.symbols_in_range(sample_file, 1, 50)?;
                    Ok(())
                }),
            ),
            (
                "branches_of",
                Box::new(|| {
                    kq.branches_of(sample_id)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.branches_of(sample_id)?;
                    Ok(())
                }),
            ),
            (
                "get_type_hierarchy",
                Box::new(|| {
                    kq.get_type_hierarchy(sample_id, 3)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.get_type_hierarchy(sample_id, 3)?;
                    Ok(())
                }),
            ),
            (
                "generate_test_context",
                Box::new(|| {
                    kq.generate_test_context(None, 10, None)?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.generate_test_context(None, 10, None)?;
                    Ok(())
                }),
            ),
            (
                "stats",
                Box::new(|| {
                    kuzu.stats()?;
                    Ok(())
                }),
                Box::new(|| {
                    cozo.stats()?;
                    Ok(())
                }),
            ),
        ];

        for (name, kuzu_fn, cozo_fn) in &queries {
            let t0 = Instant::now();
            for _ in 0..iters {
                kuzu_fn()?;
            }
            let kuzu_us = t0.elapsed().as_micros() / iters;
            let t0 = Instant::now();
            for _ in 0..iters {
                cozo_fn()?;
            }
            let cozo_us = t0.elapsed().as_micros() / iters;
            let pct = if kuzu_us > 0 {
                (cozo_us as f64 - kuzu_us as f64) / kuzu_us as f64 * 100.0
            } else {
                0.0
            };
            eprintln!(
                "{:<22} Kuzu={:>6}µs  CozoDB={:>6}µs  {:+.0}%",
                name, kuzu_us, cozo_us, pct
            );
        }
    }

    // === Incremental update test ===
    if let Some(ext) = extractions.first() {
        eprintln!("\n=== Incremental Update (re-upsert 1 file into existing DB) ===");
        let iters: u128 = 5;

        let t0 = Instant::now();
        let inc_lock = kuzu.write_lock()?;
        for _ in 0..iters {
            kuzu.upsert_file_conn(&conn, ext, &inc_lock)?;
        }
        let kuzu_inc = t0.elapsed().as_micros() / iters;

        let t0 = Instant::now();
        for _ in 0..iters {
            cozo.upsert_file(ext)?;
        }
        let cozo_inc = t0.elapsed().as_micros() / iters;

        let pct = if kuzu_inc > 0 {
            (cozo_inc as f64 - kuzu_inc as f64) / kuzu_inc as f64 * 100.0
        } else {
            0.0
        };
        eprintln!(
            "upsert_file            Kuzu={:>6}µs  CozoDB={:>6}µs  {:+.0}%",
            kuzu_inc, cozo_inc, pct
        );
    }

    // DB file sizes
    let kuzu_size = dir_size(&kuzu_path);
    let cozo_size = std::fs::metadata(&cozo_path).map(|m| m.len()).unwrap_or(0);
    eprintln!("\n=== DB Size ===");
    eprintln!("Kuzu:  {} KB", kuzu_size / 1024);
    eprintln!("CozoDB: {} KB", cozo_size / 1024);

    // Result
    eprintln!("\n=== Result ===");
    if failures == 0 {
        eprintln!("ALL CHECKS PASSED ✅");
    } else {
        eprintln!("{failures} FAILURES ❌");
    }

    std::fs::remove_dir_all(&tmp)?;

    if failures > 0 {
        anyhow::bail!("{failures} comparison failures");
    }
    Ok(())
}

fn collect_files(dir: &PathBuf, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.')
                || name == "node_modules"
                || name == "target"
                || name == "__pycache__"
            {
                continue;
            }
            collect_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

fn print_perf(label: &str, kuzu_ms: u128, cozo_ms: u128) {
    let speedup = if cozo_ms > 0 {
        kuzu_ms as f64 / cozo_ms as f64
    } else {
        f64::INFINITY
    };
    eprintln!(
        "{:<15} {:>6}ms {:>6}ms  {:.1}x",
        label, kuzu_ms, cozo_ms, speedup
    );
}

fn dir_size(path: &PathBuf) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                total += p.metadata().map(|m| m.len()).unwrap_or(0);
            } else if p.is_dir() {
                total += dir_size(&p);
            }
        }
    }
    total
}
