//! The optional SCIP metadata shared by native and other rich producers.
//! Legacy producers without it keep the importer's descriptor-based behavior.

use std::collections::{HashMap, HashSet};

use scip::types::{
    occurrence, symbol_information::Kind, Occurrence, SymbolInformation, SymbolRole,
};

use super::{occurrence_range, qualified_id, scip_owner_chain, scip_sym_to_name};

/// SCIP local ids are scoped to one Document, never to the whole Index.
pub(super) fn symbol_key(file: &str, symbol: &str) -> String {
    if symbol.starts_with("local ") {
        format!("{file}\0{symbol}")
    } else {
        symbol.to_owned()
    }
}

pub(super) fn local_id(
    file: &str,
    info: &SymbolInformation,
    infos: &HashMap<&str, &SymbolInformation>,
) -> Option<String> {
    if info.display_name.is_empty()
        || !matches!(
            info.kind.enum_value_or_default(),
            Kind::Variable
                | Kind::Constant
                | Kind::Function
                | Kind::Class
                | Kind::Struct
                | Kind::Enum
        )
    {
        return None;
    }
    let mut owners = Vec::new();
    let mut owner = info.enclosing_symbol.as_str();
    let mut seen = HashSet::new();
    while owner.starts_with("local ") {
        if !seen.insert(owner) {
            return None;
        }
        let parent = infos.get(owner)?;
        if !parent.display_name.is_empty() {
            owners.push(parent.display_name.clone());
        }
        owner = &parent.enclosing_symbol;
    }
    owners.reverse();
    if !owner.is_empty() && !owner.ends_with('/') {
        let mut outer = scip_owner_chain(file, owner);
        outer.push(scip_sym_to_name(owner));
        outer.extend(owners);
        owners = outer;
    }
    Some(qualified_id(file, &owners, &info.display_name))
}

pub(super) fn documentation(info: Option<&&SymbolInformation>) -> String {
    let Some(info) = info else {
        return String::new();
    };
    let mut text = info.documentation.join("\n\n");
    if let Some(signature) = info
        .signature_documentation
        .as_ref()
        .filter(|s| !s.text.is_empty())
    {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!(
            "```{}\n{}\n```",
            signature.language, signature.text
        ));
    }
    text
}

/// A producer's declaration extent improves SCIP-only rows; it never shrinks
/// the tree-sitter extent of a row the importer matched.
pub(super) fn declaration_range(occ: &Occurrence) -> (i32, i32, i32, i32) {
    match &occ.typed_enclosing_range {
        Some(occurrence::Typed_enclosing_range::SingleLineEnclosingRange(r)) => {
            (r.line, r.start_character, r.line, r.end_character)
        }
        Some(occurrence::Typed_enclosing_range::MultiLineEnclosingRange(r)) => {
            (r.start_line, r.start_character, r.end_line, r.end_character)
        }
        _ => match occ.enclosing_range.as_slice() {
            [sl, sc, el, ec] => (*sl, *sc, *el, *ec),
            [line, sc, ec] => (*line, *sc, *line, *ec),
            _ => occurrence_range(occ),
        },
    }
}

pub(super) fn is_container(kind: &str) -> bool {
    ["Function", "Method", "Test", "Class", "Struct", "Module"]
        .iter()
        .any(|candidate| kind.eq_ignore_ascii_case(candidate))
}

pub(super) fn reference_tables(kind: Option<Kind>, roles: i32) -> &'static [&'static str] {
    if roles & SymbolRole::Import as i32 != 0 {
        return &[];
    }
    let write = roles & SymbolRole::WriteAccess as i32 != 0;
    let read = roles & SymbolRole::ReadAccess as i32 != 0;
    // rust-analyzer sends no access flags on its references. A variable,
    // constant, property, field or parameter reference is therefore a READS
    // edge by its symbol's kind, not by the flag, so a constant read is kept.
    if write
        || read
        || matches!(
            kind,
            Some(Kind::Variable | Kind::Constant | Kind::Property | Kind::Field | Kind::Parameter)
        )
    {
        return match (write, read) {
            (true, true) => &["WRITES", "READS"],
            (true, false) => &["WRITES"],
            _ => &["READS"],
        };
    }
    // A type is named, not called: a function that returns or constructs one
    // depends on it, so the reference is its own table, not a CALLS edge.
    // Files, namespaces, modules and type parameters are not reference targets.
    // The legacy producer supplies no kind; its old CALLS behavior is retained.
    match kind {
        Some(
            Kind::Class
            | Kind::Interface
            | Kind::Trait
            | Kind::Struct
            | Kind::Enum
            | Kind::TypeAlias,
        ) => &["REFERENCES"],
        Some(Kind::File | Kind::Namespace | Kind::Module | Kind::TypeParameter) => &[],
        _ => &["CALLS"],
    }
}

pub(super) fn implementation_table(source: Option<Kind>, target: Option<Kind>) -> &'static str {
    match (source, target) {
        (Some(Kind::Class | Kind::Struct), Some(Kind::Interface | Kind::Trait)) => "IMPLEMENTS",
        (Some(Kind::Method | Kind::Property | Kind::Field | Kind::Getter | Kind::Setter), _) => {
            "IMPLEMENTS"
        }
        _ => "INHERITS",
    }
}
