mod entities;
mod relations;
pub use entities::extract_entities;
pub use relations::extract_relations;

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::lang::{LanguagePack, ParserBackend};
use crate::model::{FileExtraction, Relation, RelationKind, Span, SymbolKind};

/// Parse a source file and extract all symbols and relationships.
pub fn extract_file(path: &str, source: &[u8], pack: &LanguagePack) -> Result<FileExtraction> {
    let (symbols, mut relations) = match &pack.backend {
        ParserBackend::TreeSitter {
            grammar,
            entity_query,
            relation_query,
        } => {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(grammar)?;

            let tree = parser
                .parse(source, None)
                .ok_or_else(|| anyhow::anyhow!("failed to parse {}", path))?;

            let root = tree.root_node();

            let symbols = extract_entities(path, source, root, entity_query, &pack.name);
            let relations = extract_relations(path, source, root, relation_query);
            (symbols, relations)
        }
        ParserBackend::Custom(extractor) => extractor.extract(path, source, &pack.name)?,
    };

    // Generate CALLS edges from Route symbols to their handler functions
    generate_route_handler_edges(path, &symbols, &mut relations);

    let content_hash = {
        let mut hasher = Sha256::new();
        hasher.update(source);
        format!("{:x}", hasher.finalize())
    };

    Ok(FileExtraction {
        file: path.to_string(),
        language: pack.name.clone(),
        content_hash,
        symbols,
        relations,
    })
}

/// Create CALLS relations from Route symbols to handler functions in the same file.
/// Matches route handler names from docstrings OR route names containing function names.
fn generate_route_handler_edges(
    file: &str,
    symbols: &[crate::model::Symbol],
    relations: &mut Vec<Relation>,
) {
    // Collect function/method names for matching
    let functions: Vec<(&str, &str)> = symbols
        .iter()
        .filter(|s| {
            (s.kind == SymbolKind::Function || s.kind == SymbolKind::Method) && s.span.file == file
        })
        .map(|s| (s.name.as_str(), s.id.as_str()))
        .collect();

    for sym in symbols {
        if sym.kind != SymbolKind::Route {
            continue;
        }

        let mut target_id: Option<String> = None;

        // Method 1: explicit handler= in docstring
        if let Some(doc) = &sym.docstring {
            if let Some(handler_name) = doc.split("handler=").nth(1).map(|h| h.trim()) {
                target_id = functions
                    .iter()
                    .find(|(name, _)| *name == handler_name)
                    .map(|(_, id)| id.to_string());
            }
        }

        // Method 2: Route is on the same line range as a function — check for overlap
        if target_id.is_none() {
            target_id = symbols
                .iter()
                .find(|s| {
                    (s.kind == SymbolKind::Function || s.kind == SymbolKind::Method)
                        && s.span.file == file
                        && s.span.start_line <= sym.span.end_line
                        && s.span.end_line >= sym.span.start_line
                })
                .map(|s| s.id.clone());
        }

        if let Some(tid) = target_id {
            if tid != sym.id {
                relations.push(Relation {
                    source_id: sym.id.clone(),
                    target_id: tid,
                    kind: RelationKind::Calls,
                    span: Some(Span {
                        file: file.to_string(),
                        start_line: sym.span.start_line,
                        start_col: sym.span.start_col,
                        end_line: sym.span.end_line,
                        end_col: sym.span.end_col,
                    }),
                });
            }
        }
    }
}
