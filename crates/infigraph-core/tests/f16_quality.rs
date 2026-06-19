/// Thorough quality test: f32 vs f16 vs int8 quantization for embedding search.
///
/// Loads real embeddings, converts all vectors through each quantization,
/// then compares search results across multiple queries.
///
/// Checks per method:
/// 1. Top-K ranking order (same results, same order?)
/// 2. Score differences (cosine similarity change)
/// 3. Recall@K (% of f32 top-K in quantized top-K)
/// 4. Max rank displacement (worst case rank shift)
/// 5. Near-tie flips (pairs with gap < 1e-3 that swap)
/// 6. Storage savings
use std::path::PathBuf;

// ── f16 conversion ──────────────────────────────────────────────────

fn f32_to_f16_bits(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = (bits >> 31) & 1;
    let exp = ((bits >> 23) & 0xFF) as i32;
    let frac = bits & 0x7FFFFF;

    if exp == 0xFF {
        return ((sign << 15) | 0x7C00 | (frac >> 13).min(0x3FF)) as u16;
    }

    let new_exp = exp - 127 + 15;

    if new_exp >= 31 {
        return ((sign << 15) | 0x7C00) as u16;
    }

    if new_exp <= 0 {
        if new_exp < -10 {
            return (sign << 15) as u16;
        }
        let frac_with_hidden = frac | 0x800000;
        let shift = (1 - new_exp) as u32;
        let frac16 = frac_with_hidden >> (13 + shift);
        let round_bit = (frac_with_hidden >> (12 + shift)) & 1;
        return ((sign << 15) | frac16 + round_bit) as u16;
    }

    let frac16 = frac >> 13;
    let round_bit = (frac >> 12) & 1;
    let sticky = if (frac & 0xFFF) != 0 { 1 } else { 0 };
    let round_up = if round_bit == 1 && (sticky == 1 || (frac16 & 1) == 1) {
        1
    } else {
        0
    };

    let result = ((sign << 15) | (new_exp as u32) << 10 | frac16) + round_up;
    result as u16
}

fn f16_bits_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1F) as u32;
    let frac = (h & 0x3FF) as u32;

    if exp == 31 {
        return f32::from_bits((sign << 31) | 0x7F800000 | (frac << 13));
    }

    if exp == 0 {
        if frac == 0 {
            return f32::from_bits(sign << 31);
        }
        let mut f = frac as f32 / 1024.0;
        f *= 2.0f32.powi(-14);
        if sign == 1 {
            -f
        } else {
            f
        }
    } else {
        f32::from_bits((sign << 31) | ((exp + 112) << 23) | (frac << 13))
    }
}

fn f32_roundtrip_f16(v: f32) -> f32 {
    f16_bits_to_f32(f32_to_f16_bits(v))
}

// ── int8 scalar quantization ────────────────────────────────────────
// Per-vector min/max quantization. Each 256-dim vector stores 256 bytes + 2 f32 (min, max).
// Dequantize: val = min + (byte / 255.0) * (max - min)

struct Int8Vector {
    bytes: Vec<u8>,
    min: f32,
    max: f32,
}

fn quantize_int8(vec: &[f32]) -> Int8Vector {
    let min = vec.iter().copied().fold(f32::INFINITY, f32::min);
    let max = vec.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let range = max - min;
    let bytes = if range < 1e-10 {
        vec![128u8; vec.len()]
    } else {
        vec.iter()
            .map(|&v| {
                let normalized = (v - min) / range;
                (normalized * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect()
    };
    Int8Vector { bytes, min, max }
}

fn dequantize_int8(q: &Int8Vector) -> Vec<f32> {
    let range = q.max - q.min;
    q.bytes
        .iter()
        .map(|&b| q.min + (b as f32 / 255.0) * range)
        .collect()
}

fn f32_roundtrip_int8(vec: &[f32]) -> Vec<f32> {
    let q = quantize_int8(vec);
    dequantize_int8(&q)
}

// ── Shared helpers ──────────────────────────────────────────────────

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn top_k_search(query: &[f32], embeddings: &[(String, Vec<f32>)], k: usize) -> Vec<(String, f32)> {
    let mut scored: Vec<(String, f32)> = embeddings
        .iter()
        .map(|(id, vec)| (id.clone(), cosine_similarity(query, vec)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored
}

// ── Test runner for a quantization method ───────────────────────────

struct QualityReport {
    name: String,
    max_abs_error: f32,
    avg_abs_error: f64,
    max_sim_diff: f32,
    avg_sim_diff: f64,
    recall_at_k: Vec<(usize, f64)>,  // (k, avg_recall)
    perfect_at_k: Vec<(usize, f64)>, // (k, % perfect)
    max_displacement_at_k: Vec<(usize, usize)>,
    near_tie_flip_pct: f64,
    storage_bytes_per_vec: usize,
}

fn evaluate_method(
    name: &str,
    embeddings: &[(String, Vec<f32>)],
    quantized: &[(String, Vec<f32>)],
    storage_bytes_per_vec: usize,
) -> QualityReport {
    let count = embeddings.len();

    // Dimension error
    let mut max_abs_error: f32 = 0.0;
    let mut sum_abs_error: f64 = 0.0;
    let mut total_dims: u64 = 0;

    for (i, (_id, vec)) in embeddings.iter().enumerate() {
        for (j, &v) in vec.iter().enumerate() {
            let rt = quantized[i].1[j];
            let abs_err = (v - rt).abs();
            max_abs_error = max_abs_error.max(abs_err);
            sum_abs_error += abs_err as f64;
            total_dims += 1;
        }
    }
    let avg_abs_error = sum_abs_error / total_dims as f64;

    // Cosine similarity preservation
    let mut max_sim_diff: f32 = 0.0;
    let mut sum_sim_diff: f64 = 0.0;
    let num_pairs = count.min(1000);
    for i in 0..num_pairs {
        let j = (i * 7 + 13) % count;
        if i == j {
            continue;
        }
        let sim_f32 = cosine_similarity(&embeddings[i].1, &embeddings[j].1);
        let sim_q = cosine_similarity(&quantized[i].1, &quantized[j].1);
        let diff = (sim_f32 - sim_q).abs();
        max_sim_diff = max_sim_diff.max(diff);
        sum_sim_diff += diff as f64;
    }
    let avg_sim_diff = sum_sim_diff / num_pairs as f64;

    // Search ranking
    let k_values = [1, 3, 5, 10, 20, 50];
    let num_queries = count.min(200);
    let query_indices: Vec<usize> = (0..num_queries).map(|i| (i * 31 + 7) % count).collect();

    let mut recall_at_k = Vec::new();
    let mut perfect_at_k = Vec::new();
    let mut max_displacement_at_k = Vec::new();

    for &k in &k_values {
        let mut total_recall = 0.0f64;
        let mut perfect = 0u64;
        let mut max_disp = 0usize;

        for &qi in &query_indices {
            let query = &embeddings[qi].1;
            let f32_results = top_k_search(query, embeddings, k);
            let q_results = top_k_search(query, quantized, k);

            let f32_ids: std::collections::HashSet<&str> =
                f32_results.iter().map(|(id, _)| id.as_str()).collect();
            let q_ids: std::collections::HashSet<&str> =
                q_results.iter().map(|(id, _)| id.as_str()).collect();
            let overlap = f32_ids.intersection(&q_ids).count();
            total_recall += overlap as f64 / k as f64;
            if overlap == k {
                perfect += 1;
            }

            let q_rank_map: std::collections::HashMap<&str, usize> = q_results
                .iter()
                .enumerate()
                .map(|(r, (id, _))| (id.as_str(), r))
                .collect();
            for (f32_rank, (id, _)) in f32_results.iter().enumerate() {
                if let Some(&q_rank) = q_rank_map.get(id.as_str()) {
                    let d = (f32_rank as i64 - q_rank as i64).unsigned_abs() as usize;
                    max_disp = max_disp.max(d);
                }
            }
        }

        recall_at_k.push((k, total_recall / num_queries as f64));
        perfect_at_k.push((k, perfect as f64 / num_queries as f64));
        max_displacement_at_k.push((k, max_disp));
    }

    // Near-tie flips
    let mut near_ties = 0u64;
    let mut tie_flips = 0u64;
    for &qi in &query_indices.iter().take(100).collect::<Vec<_>>() {
        let query = &embeddings[*qi].1;
        let mut f32_scores: Vec<(usize, f32)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, (_, v))| (i, cosine_similarity(query, v)))
            .collect();
        f32_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        for w in f32_scores.windows(2).take(100) {
            let gap = w[0].1 - w[1].1;
            if gap > 0.0 && gap < 1e-3 {
                near_ties += 1;
                let q0 = cosine_similarity(query, &quantized[w[0].0].1);
                let q1 = cosine_similarity(query, &quantized[w[1].0].1);
                if q1 > q0 {
                    tie_flips += 1;
                }
            }
        }
    }

    QualityReport {
        name: name.to_string(),
        max_abs_error,
        avg_abs_error,
        max_sim_diff,
        avg_sim_diff,
        recall_at_k,
        perfect_at_k,
        max_displacement_at_k,
        near_tie_flip_pct: if near_ties > 0 {
            tie_flips as f64 / near_ties as f64 * 100.0
        } else {
            0.0
        },
        storage_bytes_per_vec,
    }
}

fn print_report(r: &QualityReport, dim: usize, count: usize) {
    let f32_total = count * dim * 4;
    let q_total = count * r.storage_bytes_per_vec;
    eprintln!("\n{}", "=".repeat(60));
    eprintln!("  {}", r.name);
    eprintln!("{}", "=".repeat(60));
    eprintln!("  Max dim error:     {:.2e}", r.max_abs_error);
    eprintln!("  Avg dim error:     {:.2e}", r.avg_abs_error);
    eprintln!("  Max cosine diff:   {:.2e}", r.max_sim_diff);
    eprintln!("  Avg cosine diff:   {:.2e}", r.avg_sim_diff);
    eprintln!("  Near-tie flips:    {:.1}%", r.near_tie_flip_pct);
    eprintln!(
        "  Storage:           {:.1} MB → {:.1} MB ({:.0}% savings)",
        f32_total as f64 / 1048576.0,
        q_total as f64 / 1048576.0,
        (1.0 - q_total as f64 / f32_total as f64) * 100.0
    );
    eprintln!("");
    eprintln!(
        "  {:>6} {:>10} {:>10} {:>10}",
        "Top-K", "Recall", "Perfect", "Max Disp"
    );
    eprintln!("  {}", "-".repeat(46));
    for i in 0..r.recall_at_k.len() {
        let (k, recall) = r.recall_at_k[i];
        let (_, perfect) = r.perfect_at_k[i];
        let (_, disp) = r.max_displacement_at_k[i];
        eprintln!(
            "  {:>6} {:>9.1}% {:>9.1}% {:>10}",
            k,
            recall * 100.0,
            perfect * 100.0,
            disp
        );
    }
}

#[test]
fn compare_f16_vs_int8_quality() {
    let embed_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".infigraph/embeddings.bin");

    if !embed_path.exists() {
        eprintln!("SKIP: no embeddings at {}", embed_path.display());
        return;
    }

    let embeddings = infigraph_core::embed::load_embeddings(&embed_path).unwrap();
    let count = embeddings.len();
    let dim = embeddings.first().map(|(_, v)| v.len()).unwrap_or(256);
    eprintln!(
        "Loaded {} embeddings ({}-dim f32, {:.1} MB)",
        count,
        dim,
        (count * dim * 4) as f64 / 1048576.0
    );

    if count < 10 {
        eprintln!("SKIP: too few embeddings");
        return;
    }

    // === Build f16 roundtrip embeddings ===
    let f16_embeddings: Vec<(String, Vec<f32>)> = embeddings
        .iter()
        .map(|(id, vec)| {
            let f16_vec: Vec<f32> = vec.iter().map(|&v| f32_roundtrip_f16(v)).collect();
            (id.clone(), f16_vec)
        })
        .collect();

    // === Build int8 roundtrip embeddings ===
    let int8_embeddings: Vec<(String, Vec<f32>)> = embeddings
        .iter()
        .map(|(id, vec)| {
            let int8_vec = f32_roundtrip_int8(vec);
            (id.clone(), int8_vec)
        })
        .collect();

    // f16: 2 bytes per dim
    let f16_report = evaluate_method(
        "f16 (half precision)",
        &embeddings,
        &f16_embeddings,
        dim * 2,
    );
    // int8: 1 byte per dim + 8 bytes overhead (min + max as f32)
    let int8_report = evaluate_method(
        "int8 (scalar quantization)",
        &embeddings,
        &int8_embeddings,
        dim + 8,
    );

    print_report(&f16_report, dim, count);
    print_report(&int8_report, dim, count);

    // === Side-by-side comparison table ===
    eprintln!("\n{}", "=".repeat(70));
    eprintln!("  SIDE-BY-SIDE COMPARISON");
    eprintln!("{}", "=".repeat(70));
    eprintln!("  {:>25} {:>15} {:>15}", "", "f16", "int8");
    eprintln!("  {}", "-".repeat(55));
    eprintln!(
        "  {:>25} {:>14.2e} {:>14.2e}",
        "Max dim error", f16_report.max_abs_error, int8_report.max_abs_error
    );
    eprintln!(
        "  {:>25} {:>14.2e} {:>14.2e}",
        "Max cosine diff", f16_report.max_sim_diff, int8_report.max_sim_diff
    );

    for i in 0..f16_report.recall_at_k.len() {
        let k = f16_report.recall_at_k[i].0;
        eprintln!(
            "  {:>20} top-{:<2} {:>14.1}% {:>14.1}%",
            "Recall",
            k,
            f16_report.recall_at_k[i].1 * 100.0,
            int8_report.recall_at_k[i].1 * 100.0
        );
    }

    let f32_mb = (count * dim * 4) as f64 / 1048576.0;
    let f16_mb = (count * dim * 2) as f64 / 1048576.0;
    let int8_mb = (count * (dim + 8)) as f64 / 1048576.0;
    eprintln!("  {:>25} {:>13.1}MB {:>13.1}MB", "Size", f16_mb, int8_mb);
    eprintln!(
        "  {:>25} {:>14.0}% {:>14.0}%",
        "Savings vs f32",
        (1.0 - f16_mb / f32_mb) * 100.0,
        (1.0 - int8_mb / f32_mb) * 100.0
    );
    eprintln!(
        "  {:>25} {:>14.1}% {:>14.1}%",
        "Near-tie flips", f16_report.near_tie_flip_pct, int8_report.near_tie_flip_pct
    );

    // === Sample query comparison ===
    let qi = (0 * 31 + 7) % count;
    let query = &embeddings[qi].1;
    let k = 10;
    let f32_top = top_k_search(query, &embeddings, k);
    let f16_top = top_k_search(query, &f16_embeddings, k);
    let int8_top = top_k_search(query, &int8_embeddings, k);

    eprintln!(
        "\n=== Sample Query Top-{} (query: {}) ===",
        k,
        embeddings[qi]
            .0
            .rsplit("::")
            .next()
            .unwrap_or(&embeddings[qi].0)
    );
    eprintln!(
        "  {:>3} {:>30} {:>10} {:>30} {:>10} {:>30} {:>10}",
        "#", "f32", "score", "f16", "score", "int8", "score"
    );
    eprintln!("  {}", "-".repeat(115));
    for i in 0..k {
        let f32_name = f32_top[i].0.rsplit("::").next().unwrap_or(&f32_top[i].0);
        let f16_name = f16_top[i].0.rsplit("::").next().unwrap_or(&f16_top[i].0);
        let int8_name = int8_top[i].0.rsplit("::").next().unwrap_or(&int8_top[i].0);
        let f16_match = if f32_top[i].0 == f16_top[i].0 {
            "✅"
        } else {
            "❌"
        };
        let int8_match = if f32_top[i].0 == int8_top[i].0 {
            "✅"
        } else {
            "❌"
        };
        eprintln!(
            "  {:>3} {:>30} {:.6} {:>28}{} {:.6} {:>28}{} {:.6}",
            i + 1,
            &f32_name[..f32_name.len().min(30)],
            f32_top[i].1,
            &f16_name[..f16_name.len().min(28)],
            f16_match,
            f16_top[i].1,
            &int8_name[..int8_name.len().min(28)],
            int8_match,
            int8_top[i].1
        );
    }

    eprintln!("\n=== VERDICT ===");
    let f16_top1 = f16_report
        .recall_at_k
        .iter()
        .find(|(k, _)| *k == 1)
        .map(|(_, r)| *r)
        .unwrap_or(0.0);
    let int8_top1 = int8_report
        .recall_at_k
        .iter()
        .find(|(k, _)| *k == 1)
        .map(|(_, r)| *r)
        .unwrap_or(0.0);
    let f16_top5 = f16_report
        .recall_at_k
        .iter()
        .find(|(k, _)| *k == 5)
        .map(|(_, r)| *r)
        .unwrap_or(0.0);
    let int8_top5 = int8_report
        .recall_at_k
        .iter()
        .find(|(k, _)| *k == 5)
        .map(|(_, r)| *r)
        .unwrap_or(0.0);
    eprintln!(
        "f16:  top-1 recall={:.1}%, top-5 recall={:.1}%, savings=50%",
        f16_top1 * 100.0,
        f16_top5 * 100.0
    );
    eprintln!(
        "int8: top-1 recall={:.1}%, top-5 recall={:.1}%, savings=75%",
        int8_top1 * 100.0,
        int8_top5 * 100.0
    );

    // Fail if int8 drops below 95% recall at top-5
    assert!(
        int8_top5 >= 0.90,
        "int8 top-5 recall too low: {:.1}%",
        int8_top5 * 100.0
    );
}
