use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use anyhow::Result;

/// A single learned resolution pattern — records how SCIP corrected a
/// tree-sitter heuristic so the correction can be replayed in future
/// indexes even without SCIP data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedPattern {
    /// File where the call occurs (relative path).
    pub source_file: String,
    /// Name of the called function/method.
    pub call_name: String,
    /// Correct target file (relative path).
    pub resolved_to_file: String,
    /// Correct target symbol id.
    pub resolved_to_symbol: String,
    /// Confidence score in 0.0..=1.0 — increases with repeated corrections.
    pub confidence: f32,
    /// Origin of the pattern: "scip" or "user".
    pub source: String,
    /// Unix-epoch seconds when this pattern was last updated.
    pub last_updated: String,
}

/// Persistent store for learned resolution patterns.
///
/// Stored as `.infigraph/learned/patterns.json`, separate from the graph DB
/// so it survives `infigraph index --full` and graph rebuilds.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LearnedStore {
    pub patterns: Vec<LearnedPattern>,
}

impl LearnedStore {
    /// Load from `.infigraph/learned/patterns.json`.
    /// Returns an empty store if the file doesn't exist or is malformed.
    pub fn load(root: &Path) -> Self {
        let path = Self::path(root);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist to `.infigraph/learned/patterns.json`.
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Self::path(root);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    fn path(root: &Path) -> PathBuf {
        root.join(".infigraph")
            .join("learned")
            .join("patterns.json")
    }

    /// Record a correction: SCIP resolved a call differently than tree-sitter.
    ///
    /// If a pattern for the same (source_file, call_name) already exists its
    /// confidence is bumped by 0.1 (capped at 1.0). Otherwise a new pattern
    /// is created with confidence 0.5.
    pub fn record_correction(
        &mut self,
        source_file: &str,
        call_name: &str,
        resolved_to_file: &str,
        resolved_to_symbol: &str,
    ) {
        if let Some(existing) = self.patterns.iter_mut().find(|p| {
            p.source_file == source_file && p.call_name == call_name
        }) {
            existing.resolved_to_file = resolved_to_file.to_string();
            existing.resolved_to_symbol = resolved_to_symbol.to_string();
            existing.confidence = (existing.confidence + 0.1).min(1.0);
            existing.last_updated = epoch_now();
        } else {
            self.patterns.push(LearnedPattern {
                source_file: source_file.to_string(),
                call_name: call_name.to_string(),
                resolved_to_file: resolved_to_file.to_string(),
                resolved_to_symbol: resolved_to_symbol.to_string(),
                confidence: 0.5,
                source: "scip".to_string(),
                last_updated: epoch_now(),
            });
        }
    }

    /// Look up a learned resolution for a call site.
    /// Only returns patterns with confidence >= 0.3.
    pub fn lookup(&self, source_file: &str, call_name: &str) -> Option<&LearnedPattern> {
        self.patterns.iter().find(|p| {
            p.source_file == source_file
                && p.call_name == call_name
                && p.confidence >= 0.3
        })
    }

    /// Remove patterns pointing to files that no longer exist in the index.
    pub fn prune_stale(&mut self, existing_files: &std::collections::HashSet<String>) {
        self.patterns
            .retain(|p| existing_files.contains(&p.resolved_to_file));
    }

    /// Clear all learned data.
    pub fn clear(&mut self) {
        self.patterns.clear();
    }

    /// Number of stored patterns.
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// Simple epoch-seconds timestamp without pulling in the `chrono` crate.
fn epoch_now() -> String {
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_record_and_lookup() {
        let mut store = LearnedStore::default();
        store.record_correction("main.py", "authenticate", "auth.py", "auth.py::authenticate");

        let found = store.lookup("main.py", "authenticate");
        assert!(found.is_some());
        let p = found.unwrap();
        assert_eq!(p.resolved_to_symbol, "auth.py::authenticate");
        assert!((p.confidence - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_repeated_correction_increases_confidence() {
        let mut store = LearnedStore::default();
        store.record_correction("main.py", "authenticate", "auth.py", "auth.py::authenticate");
        store.record_correction("main.py", "authenticate", "auth.py", "auth.py::authenticate");
        store.record_correction("main.py", "authenticate", "auth.py", "auth.py::authenticate");

        let p = store.lookup("main.py", "authenticate").unwrap();
        assert!((p.confidence - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn test_confidence_caps_at_one() {
        let mut store = LearnedStore::default();
        for _ in 0..20 {
            store.record_correction("a.py", "foo", "b.py", "b.py::foo");
        }
        let p = store.lookup("a.py", "foo").unwrap();
        assert!((p.confidence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_low_confidence_not_returned() {
        let mut store = LearnedStore::default();
        store.patterns.push(LearnedPattern {
            source_file: "a.py".into(),
            call_name: "foo".into(),
            resolved_to_file: "b.py".into(),
            resolved_to_symbol: "b.py::foo".into(),
            confidence: 0.1,
            source: "scip".into(),
            last_updated: "0".into(),
        });
        assert!(store.lookup("a.py", "foo").is_none());
    }

    #[test]
    fn test_save_and_load() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let mut store = LearnedStore::default();
        store.record_correction("main.py", "auth", "auth.py", "auth.py::auth");
        store.save(root).unwrap();

        let loaded = LearnedStore::load(root);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.patterns[0].call_name, "auth");
    }

    #[test]
    fn test_prune_stale() {
        let mut store = LearnedStore::default();
        store.record_correction("a.py", "foo", "b.py", "b.py::foo");
        store.record_correction("a.py", "bar", "c.py", "c.py::bar");

        let mut existing = std::collections::HashSet::new();
        existing.insert("b.py".to_string());
        store.prune_stale(&existing);

        assert_eq!(store.len(), 1);
        assert_eq!(store.patterns[0].call_name, "foo");
    }

    #[test]
    fn test_clear() {
        let mut store = LearnedStore::default();
        store.record_correction("a.py", "foo", "b.py", "b.py::foo");
        assert_eq!(store.len(), 1);
        store.clear();
        assert_eq!(store.len(), 0);
    }
}
