use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use infigraph_core::graph::CozoStore;
use serde::Serialize;
use sha2::{Digest, Sha256};

fn main() -> Result<()> {
    let project_root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    let cozo_path = project_root.join(".infigraph/graph.cozo");
    if !cozo_path.exists() {
        anyhow::bail!("No CozoDB graph at {}", cozo_path.display());
    }

    let store = CozoStore::open(&cozo_path)?;

    let out_dir = project_root.join(".infigraph/golden_cozo");
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir)?;
    }
    std::fs::create_dir_all(&out_dir)?;

    let stats = store.stats()?;
    let mut manifest = Manifest {
        timestamp: chrono::Utc::now().to_rfc3339(),
        graph_symbols: stats.symbols as usize,
        entries: Vec::new(),
    };

    let sample = pick_samples(&store)?;
    eprintln!(
        "Samples: {} symbol IDs, {} files",
        sample.symbol_ids.len(),
        sample.files.len()
    );

    // 1. symbols_in_file
    {
        let mut all = BTreeMap::new();
        for file in &sample.files {
            let mut rows = store.symbols_in_file(file)?;
            rows.sort_by(|a, b| a.id.cmp(&b.id));
            all.insert(file.clone(), rows);
        }
        write_golden(&out_dir, &mut manifest, "symbols_in_file", &all)?;
    }

    // 2. callers_of
    {
        let mut all = BTreeMap::new();
        for id in &sample.symbol_ids {
            let mut callers = store.callers_of(id)?;
            callers.sort();
            all.insert(id.clone(), callers);
        }
        write_golden(&out_dir, &mut manifest, "callers_of", &all)?;
    }

    // 3. callees_of
    {
        let mut all = BTreeMap::new();
        for id in &sample.symbol_ids {
            let mut callees = store.callees_of(id)?;
            callees.sort();
            all.insert(id.clone(), callees);
        }
        write_golden(&out_dir, &mut manifest, "callees_of", &all)?;
    }

    // 4. branches_of
    {
        let mut all = BTreeMap::new();
        for id in &sample.symbol_ids {
            let branches = store.branches_of(id)?;
            all.insert(id.clone(), branches);
        }
        write_golden(&out_dir, &mut manifest, "branches_of", &all)?;
    }

    // 5. transitive_impact
    {
        let mut all = BTreeMap::new();
        for id in sample.symbol_ids.iter().take(3) {
            let mut rows = store.transitive_impact(id, 3)?;
            rows.sort_by(|a, b| a.id.cmp(&b.id));
            all.insert(id.clone(), rows);
        }
        write_golden(&out_dir, &mut manifest, "transitive_impact", &all)?;
    }

    // 6. symbols_in_range
    {
        let mut all = BTreeMap::new();
        for file in &sample.files {
            let mut rows = store.symbols_in_range(file, 1, 50)?;
            rows.sort_by(|a, b| a.id.cmp(&b.id));
            all.insert(file.clone(), rows);
        }
        write_golden(&out_dir, &mut manifest, "symbols_in_range", &all)?;
    }

    // 7. find_symbol_by_id
    {
        let mut all = BTreeMap::new();
        for id in &sample.symbol_ids {
            let detail = store.find_symbol_by_id(id)?;
            all.insert(id.clone(), detail);
        }
        all.insert(
            "__nonexistent__".to_string(),
            store.find_symbol_by_id("__nonexistent__")?,
        );
        write_golden(&out_dir, &mut manifest, "find_symbol_by_id", &all)?;
    }

    // 8. find_all_references
    {
        let mut all = BTreeMap::new();
        for id in &sample.symbol_ids {
            let mut refs = store.find_all_references(id)?;
            refs.sort_by(|a, b| a.caller_id.cmp(&b.caller_id).then(a.line.cmp(&b.line)));
            all.insert(id.clone(), refs);
        }
        write_golden(&out_dir, &mut manifest, "find_all_references", &all)?;
    }

    // 9. get_api_surface
    {
        let mut rows = store.get_api_surface()?;
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        write_golden(&out_dir, &mut manifest, "get_api_surface", &rows)?;
    }

    // 10. get_file_deps
    {
        let mut all = BTreeMap::new();
        for file in &sample.files {
            let mut deps = store.get_file_deps(file)?;
            deps.imports.sort();
            deps.imported_by.sort();
            all.insert(file.clone(), deps);
        }
        write_golden(&out_dir, &mut manifest, "get_file_deps", &all)?;
    }

    // 11. get_type_hierarchy
    {
        let mut all = BTreeMap::new();
        for id in &sample.inherits_ids {
            let mut h = store.get_type_hierarchy(id, 5)?;
            h.ancestors.sort_by(|a, b| a.id.cmp(&b.id));
            h.descendants.sort_by(|a, b| a.id.cmp(&b.id));
            all.insert(id.clone(), h);
        }
        write_golden(&out_dir, &mut manifest, "get_type_hierarchy", &all)?;
    }

    // 12. get_test_coverage
    {
        let mut cov = store.get_test_coverage()?;
        cov.covered.sort_by(|a, b| {
            a.symbol_id
                .cmp(&b.symbol_id)
                .then(a.test_id.cmp(&b.test_id))
        });
        cov.uncovered.sort_by(|a, b| a.symbol_id.cmp(&b.symbol_id));
        write_golden(&out_dir, &mut manifest, "get_test_coverage", &cov)?;
    }

    // 13. generate_test_context
    {
        let mut ctx = store.generate_test_context(None, 10)?;
        ctx.targets.sort_by(|a, b| a.symbol_id.cmp(&b.symbol_id));
        for t in &mut ctx.targets {
            t.callers.sort();
            t.callees.sort();
        }
        write_golden(&out_dir, &mut manifest, "generate_test_context", &ctx)?;
    }

    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(out_dir.join("manifest.json"), &manifest_json)?;

    eprintln!(
        "CozoDB golden export: {} queries → {}",
        manifest.entries.len(),
        out_dir.display()
    );
    for e in &manifest.entries {
        eprintln!(
            "  {} — {} bytes, hash {}",
            e.query,
            e.size_bytes,
            &e.content_hash[..16]
        );
    }

    Ok(())
}

struct Samples {
    symbol_ids: Vec<String>,
    files: Vec<String>,
    inherits_ids: Vec<String>,
}

fn pick_samples(store: &CozoStore) -> Result<Samples> {
    // Top 5 most-called symbols
    let top_called = store.raw_query(
        r#"?[target, count(caller)] := *calls{caller, callee: target}
        :order -count(caller)
        :limit 5"#,
    )?;

    // 3 symbols with zero callers
    let zero_callers = store.raw_query(
        r#"called[id] := *calls{callee: id}
        ?[id] := *symbol{id, kind}, kind in ["Function", "Method"], not called[id]
        :order id
        :limit 3"#,
    )?;

    // INHERITS participants
    let inherits = store.raw_query(r#"?[child, parent] := *inherits{child, parent}"#)?;

    let mut symbol_ids: Vec<String> = Vec::new();
    for row in &top_called {
        if let Some(id) = row.first() {
            symbol_ids.push(id.clone());
        }
    }
    for row in &zero_callers {
        if let Some(id) = row.first() {
            symbol_ids.push(id.clone());
        }
    }
    symbol_ids.sort();
    symbol_ids.dedup();

    let mut inherits_ids: Vec<String> = Vec::new();
    for row in &inherits {
        for id in row {
            inherits_ids.push(id.clone());
        }
    }
    inherits_ids.sort();
    inherits_ids.dedup();

    let mut files: Vec<String> = Vec::new();
    for id in &symbol_ids {
        if let Some(detail) = store.find_symbol_by_id(id)? {
            if !files.contains(&detail.file) {
                files.push(detail.file);
            }
        }
    }
    files.push("tests/fixtures/python-simple/models.py".to_string());
    files.sort();
    files.dedup();

    Ok(Samples {
        symbol_ids,
        files,
        inherits_ids,
    })
}

#[derive(Serialize)]
struct Manifest {
    timestamp: String,
    graph_symbols: usize,
    entries: Vec<ManifestEntry>,
}

#[derive(Serialize)]
struct ManifestEntry {
    query: String,
    file: String,
    size_bytes: usize,
    content_hash: String,
}

fn write_golden<T: Serialize>(
    out_dir: &Path,
    manifest: &mut Manifest,
    name: &str,
    data: &T,
) -> Result<()> {
    let json = serde_json::to_string_pretty(data).with_context(|| format!("serialize {name}"))?;
    let file_name = format!("{name}.json");
    let path = out_dir.join(&file_name);
    let size = json.len();

    let mut hasher = Sha256::new();
    hasher.update(json.as_bytes());
    let hash = format!("{:x}", hasher.finalize());

    std::fs::write(&path, &json)?;

    manifest.entries.push(ManifestEntry {
        query: name.to_string(),
        file: file_name,
        size_bytes: size,
        content_hash: hash,
    });

    Ok(())
}
