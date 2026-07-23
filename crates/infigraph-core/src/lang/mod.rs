mod registry;

pub use registry::LanguageRegistry;

use anyhow::Result;
use serde::Deserialize;
use tree_sitter::{Language, Query};

use crate::model::{Relation, Symbol};

/// A custom edge type that a language pack can define beyond the standard
/// CALLS/IMPORTS/INHERITS model. Custom edges are populated during extraction
/// when capture groups matching `@{capture}.source` / `@{capture}.target`
/// are found in relations.scm.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomEdgeDef {
    pub name: String,
    pub capture: String,
}

/// Trait for custom extraction backends (e.g., JVM grammar plugins).
pub trait CustomExtractor: Send + Sync {
    fn extract(
        &self,
        path: &str,
        source: &[u8],
        language: &str,
    ) -> Result<(Vec<Symbol>, Vec<Relation>)>;
}

/// Parser backend — tree-sitter or runtime-loaded custom extractor.
pub enum ParserBackend {
    TreeSitter {
        grammar: Language,
        entity_query: Query,
        relation_query: Box<Query>,
        /// Optional query for resolving a captured `@inherit.parent`/`@inherit.child`
        /// node down to its base identifier when it's a compound wrapper (generics,
        /// qualified/dotted names, member expressions). `None` for languages whose
        /// grammar can't produce such compound shapes in an inheritance position, or
        /// where a single fully-anchored pattern in `relation_query` already handles it.
        inherit_decompose_query: Option<Box<Query>>,
    },
    Custom(Box<dyn CustomExtractor>),
}

/// A language pack bundles a parser backend with file extension mappings.
pub struct LanguagePack {
    pub name: String,
    pub extensions: Vec<String>,
    pub backend: ParserBackend,
    pub custom_edges: Vec<CustomEdgeDef>,
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
        let relation_query = Box::new(Query::new(&grammar, relation_query_src)?);
        Ok(Self {
            name: name.to_string(),
            extensions: extensions.into_iter().map(String::from).collect(),
            backend: ParserBackend::TreeSitter {
                grammar,
                entity_query,
                relation_query,
                inherit_decompose_query: None,
            },
            custom_edges: Vec::new(),
        })
    }

    /// Attach a decomposition query used to resolve compound `@inherit.parent`/
    /// `@inherit.child` captures (generics, qualified names, member expressions) down
    /// to their base identifier. Only meaningful for `ParserBackend::TreeSitter` packs;
    /// a no-op on `Custom` backends.
    pub fn with_inherit_decompose(mut self, query_src: &str) -> Result<Self> {
        if let ParserBackend::TreeSitter {
            grammar,
            inherit_decompose_query,
            ..
        } = &mut self.backend
        {
            *inherit_decompose_query = Some(Box::new(Query::new(grammar, query_src)?));
        }
        Ok(self)
    }

    /// Create a tree-sitter-backed language pack with custom edge definitions.
    pub fn new_with_custom_edges(
        name: &str,
        extensions: Vec<&str>,
        grammar: Language,
        entity_query_src: &str,
        relation_query_src: &str,
        custom_edges: Vec<CustomEdgeDef>,
    ) -> Result<Self> {
        let mut pack = Self::new(
            name,
            extensions,
            grammar,
            entity_query_src,
            relation_query_src,
        )?;
        pack.custom_edges = custom_edges;
        Ok(pack)
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
            custom_edges: Vec::new(),
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
