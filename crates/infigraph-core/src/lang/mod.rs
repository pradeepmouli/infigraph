mod registry;

pub use registry::LanguageRegistry;

use anyhow::Result;
use tree_sitter::{Language, Query};

use crate::model::{Relation, Symbol};

/// Trait for custom extraction backends (e.g., JVM grammar plugins).
pub trait CustomExtractor: Send + Sync {
    fn extract(&self, path: &str, source: &[u8], language: &str) -> Result<(Vec<Symbol>, Vec<Relation>)>;
}

/// Parser backend — tree-sitter or runtime-loaded custom extractor.
pub enum ParserBackend {
    TreeSitter {
        grammar: Language,
        entity_query: Query,
        relation_query: Query,
    },
    Custom(Box<dyn CustomExtractor>),
}

/// A language pack bundles a parser backend with file extension mappings.
pub struct LanguagePack {
    pub name: String,
    pub extensions: Vec<String>,
    pub backend: ParserBackend,
}

impl LanguagePack {
    /// Create a tree-sitter-backed language pack from a grammar and raw query strings.
    pub fn new(
        name: &str,
        extensions: Vec<&str>,
        grammar: Language,
        entity_query_src: &str,
        relation_query_src: &str,
    ) -> Result<Self> {
        let entity_query = Query::new(&grammar, entity_query_src)?;
        let relation_query = Query::new(&grammar, relation_query_src)?;
        Ok(Self {
            name: name.to_string(),
            extensions: extensions.into_iter().map(String::from).collect(),
            backend: ParserBackend::TreeSitter {
                grammar,
                entity_query,
                relation_query,
            },
        })
    }

    /// Create a language pack with a custom extraction backend.
    pub fn new_custom(
        name: &str,
        extensions: Vec<String>,
        extractor: Box<dyn CustomExtractor>,
    ) -> Self {
        Self {
            name: name.to_string(),
            extensions,
            backend: ParserBackend::Custom(extractor),
        }
    }
}

impl std::fmt::Debug for LanguagePack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanguagePack")
            .field("name", &self.name)
            .field("extensions", &self.extensions)
            .finish()
    }
}
