use std::collections::HashMap;

use super::LanguagePack;

/// Probe function: given file content, returns true if this pack should handle the file.
pub type ContentProbe = fn(content: &[u8]) -> bool;

/// Registry that maps file extensions to language packs.
pub struct LanguageRegistry {
    by_extension: HashMap<String, usize>,
    packs: Vec<LanguagePack>,
    /// Extension → (pack_index, probe_fn) for content-based override.
    /// When a file matches a primary extension AND has a content probe registered,
    /// the probe decides whether to override with the probed pack.
    content_overrides: HashMap<String, (usize, ContentProbe)>,
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self {
            by_extension: HashMap::new(),
            packs: Vec::new(),
            content_overrides: HashMap::new(),
        }
    }

    /// Register a language pack. All its extensions become resolvable.
    pub fn register(&mut self, pack: LanguagePack) {
        let idx = self.packs.len();
        for ext in &pack.extensions {
            self.by_extension.insert(ext.clone(), idx);
        }
        self.packs.push(pack);
    }

    /// Register a language pack that claims extensions only when a content probe matches.
    /// The pack's own `extensions` list may be empty — the `probe_extensions` define
    /// which extensions trigger the probe. If the probe returns true, this pack overrides
    /// whatever tree-sitter pack would otherwise handle the file.
    pub fn register_with_content_probe(
        &mut self,
        pack: LanguagePack,
        probe_extensions: &[&str],
        probe: ContentProbe,
    ) {
        let idx = self.packs.len();
        for ext in &pack.extensions {
            self.by_extension.insert(ext.clone(), idx);
        }
        for ext in probe_extensions {
            self.content_overrides.insert(ext.to_string(), (idx, probe));
        }
        self.packs.push(pack);
    }

    /// Look up a language pack by file extension (e.g., ".py").
    pub fn for_extension(&self, ext: &str) -> Option<&LanguagePack> {
        self.by_extension.get(ext).map(|&idx| &self.packs[idx])
    }

    /// Look up a language pack by file path.
    pub fn for_file(&self, path: &str) -> Option<&LanguagePack> {
        let ext = path.rsplit_once('.').map(|(_, e)| format!(".{e}"))?;
        self.for_extension(&ext)
    }

    /// Look up a language pack by file path and content.
    /// If a content probe is registered for this extension and matches,
    /// returns the probed pack instead of the primary extension match.
    pub fn for_file_with_content(&self, path: &str, content: &[u8]) -> Option<&LanguagePack> {
        let ext = path.rsplit_once('.').map(|(_, e)| format!(".{e}"))?;
        if let Some(&(idx, probe)) = self.content_overrides.get(&ext) {
            if probe(content) {
                return Some(&self.packs[idx]);
            }
        }
        self.for_extension(&ext)
    }

    pub fn languages(&self) -> impl Iterator<Item = &LanguagePack> {
        self.packs.iter()
    }
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}
