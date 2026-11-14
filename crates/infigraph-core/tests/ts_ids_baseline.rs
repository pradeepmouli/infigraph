//! Test 7 for the TypeScript namespace and quoted-key change: every symbol id
//! the TypeScript extraction gave before the change is still given, as often as
//! before. A namespace is a new symbol, a quoted key is a new member; neither may
//! move an id that already existed, or the change would be an ID_SCHEME bump.
//!
//! The baseline is `tests/fixtures/ts_ids_baseline.txt`: one `file<TAB>id` line
//! per extracted symbol, duplicates kept, recorded from the extraction before the
//! change. `regenerate_baseline` rewrites it; run it only to record a deliberate
//! change to what existing files extract.

use std::collections::BTreeMap;
use std::path::Path;

use infigraph_core::extract::extract_file;

/// The TypeScript files the baseline covers: the microservices fixture and every
/// `scip_ts_*` fixture (`.tsx` too), as paths relative to `tests/fixtures`.
const FILES: &[&str] = &[
    "../../tests/fixtures/microservices/order-service/src/client.ts",
    "../../tests/fixtures/microservices/order-service/src/middleware.ts",
    "../../tests/fixtures/microservices/order-service/src/models.ts",
    "../../tests/fixtures/microservices/order-service/src/routes.ts",
    "tests/fixtures/scip_ts_literal_keys/a.ts",
    "tests/fixtures/scip_ts_member_names/a.ts",
    "tests/fixtures/scip_ts_namespace/a.ts",
    "tests/fixtures/scip_ts_properties/a.ts",
    "tests/fixtures/scip_ts_quoted_name/a.ts",
    "tests/fixtures/scip_ts_wrapped_literals/a.ts",
    "tests/fixtures/scip_ts_wrapped_literals/b.tsx",
];

const BASELINE: &str = "tests/fixtures/ts_ids_baseline.txt";

/// Every `(file, id)` the extraction gives, with how many times each occurs.
fn extracted() -> BTreeMap<(String, String), usize> {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let registry = infigraph_languages::bundled_registry().unwrap();
    let mut counts = BTreeMap::new();
    for rel in FILES {
        let source = std::fs::read(crate_dir.join(rel)).unwrap();
        let ext = if rel.ends_with(".tsx") { ".tsx" } else { ".ts" };
        let pack = registry.for_extension(ext).unwrap();
        let extracted = extract_file(rel, &source, pack).unwrap();
        for symbol in extracted.symbols {
            *counts.entry(((*rel).to_string(), symbol.id)).or_insert(0) += 1;
        }
    }
    counts
}

fn baseline() -> BTreeMap<(String, String), usize> {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(BASELINE))
        .expect("the baseline file");
    let mut counts = BTreeMap::new();
    for line in text.lines() {
        let (file, id) = line.split_once('\t').expect("file<TAB>id");
        *counts
            .entry((file.to_string(), id.to_string()))
            .or_insert(0) += 1;
    }
    counts
}

#[test]
fn every_id_the_baseline_recorded_is_still_extracted_as_often() {
    let before = baseline();
    let now = extracted();
    let mut lost = Vec::new();
    for ((file, id), count) in &before {
        let have = now.get(&(file.clone(), id.clone())).copied().unwrap_or(0);
        if have < *count {
            lost.push(format!("{file}  {id}  (was {count}, now {have})"));
        }
    }
    assert!(
        lost.is_empty(),
        "existing ids moved or disappeared; this is an ID_SCHEME change:\n  {}",
        lost.join("\n  ")
    );
}

#[test]
#[ignore = "records the baseline; run deliberately, see the module docs"]
fn regenerate_baseline() {
    let mut out = String::new();
    for ((file, id), count) in extracted() {
        for _ in 0..count {
            out.push_str(&format!("{file}\t{id}\n"));
        }
    }
    std::fs::write(Path::new(env!("CARGO_MANIFEST_DIR")).join(BASELINE), out).unwrap();
}
