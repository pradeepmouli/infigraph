//! A TypeScript fixture directory (`tests/fixtures/<name>`: sources, plus
//! `scip.bin` from scip-typescript and `scip-tsrs.bin` from scip-tsrs), read
//! through tree-sitter and through a SCIP import into a graph.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use infigraph_core::extract::extract_file;
use infigraph_core::graph::{names, GraphStore};
use infigraph_core::scip::{definition_ids, import_scip_index, use_language_registry};

pub const SCIP_FILES: [&str; 2] = ["scip.bin", "scip-tsrs.bin"];

pub struct TsFixture {
    dir: PathBuf,
}

/// `id` without a trailing `#<digits>` ordinal.
pub fn base(id: &str) -> &str {
    match id.rsplit_once('#') {
        Some((head, n)) if n.chars().all(|c| c.is_ascii_digit()) => head,
        _ => id,
    }
}

impl TsFixture {
    pub fn new(name: &str) -> Self {
        TsFixture {
            dir: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name),
        }
    }

    pub fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    /// Tree-sitter's `(id, kind)` for one file of the fixture.
    pub fn extracted(&self, file: &str) -> Vec<(String, String)> {
        let source = std::fs::read(self.path(file)).unwrap();
        let registry = infigraph_languages::bundled_registry().unwrap();
        let ext = if file.ends_with(".tsx") {
            ".tsx"
        } else {
            ".ts"
        };
        let pack = registry.for_extension(ext).unwrap();
        extract_file(file, &source, pack)
            .unwrap()
            .symbols
            .into_iter()
            .map(|s| (s.id, format!("{:?}", s.kind)))
            .collect()
    }

    pub fn ids(&self, file: &str) -> BTreeSet<String> {
        self.extracted(file).into_iter().map(|(id, _)| id).collect()
    }

    pub fn kind_of(&self, file: &str, id: &str) -> String {
        self.extracted(file)
            .into_iter()
            .find(|(i, _)| i == id)
            .map(|(_, k)| k)
            .unwrap_or_else(|| panic!("no symbol {id}"))
    }

    /// The ids `scip` (a file of the fixture) defines in `file`, repeats collapsed.
    pub fn scip_ids(&self, scip: &str, file: &str) -> BTreeSet<String> {
        let bytes = std::fs::read(self.path(scip)).unwrap();
        definition_ids(&bytes, file).unwrap().into_iter().collect()
    }

    /// The graph's `(id, kind)` rows for `file`: tree-sitter's symbols, and after
    /// importing `scip` when one is given.
    pub fn graph_rows(&self, file: &str, scip: Option<&str>) -> Vec<(String, String)> {
        use_language_registry(Arc::new(infigraph_languages::bundled_registry().unwrap()));
        let source = std::fs::read(self.path(file)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(file), &source).unwrap();
        let registry = infigraph_languages::bundled_registry().unwrap();
        let pack = registry.for_extension(".ts").unwrap();
        let extracted = extract_file(file, &source, pack).unwrap();
        let store = GraphStore::open(&dir.path().join(names::STEM)).unwrap();
        store.upsert_file(&extracted).unwrap();
        if let Some(scip) = scip {
            import_scip_index(&self.path(scip), &store, Some(dir.path())).unwrap();
        }
        let conn = store.connection().unwrap();
        let mut rows: Vec<(String, String)> = conn
            .query(&format!(
                "MATCH (s:Symbol) WHERE s.file = '{file}' RETURN s.id, s.kind"
            ))
            .unwrap()
            .map(|row| (row[0].to_string(), row[1].to_string()))
            .collect();
        rows.sort();
        rows
    }
}
