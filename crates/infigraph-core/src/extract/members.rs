//! Who owns the members of an object literal: `OXFMT_CONFIG` owns
//! `ignorePatterns` in `const OXFMT_CONFIG = { ignorePatterns: [] }`, and
//! `A::b` owns `c` in `{ b: { c: 1 } }`.
//!
//! A language's `entities.scm` says so with `@member.*` captures. They emit
//! **no symbol**: a member is a symbol only when an indexer such as SCIP finds
//! it, and this answers the one question SCIP cannot (its monikers carry no
//! owner for an object literal, only a per-file counter). Nothing here runs at
//! index time and nothing is stored: the SCIP import calls [`literal_members`]
//! over the file's current text, only for a file that has such a member, so a
//! rebuild's trimmed extractions and the graph schema are unaffected.
//!
//!   @member.name   the key of a member (a pair, a shorthand, a method)
//!   @member.within the object literal that member sits in
//!   @member.root   the name a literal is bound to (`const X = {..}`)
//!   @member.owner  the key a nested literal is the value of (`b: {..}`)
//!   @member.value  the expression a root or an owner is bound to: the literal
//!                  it names is found by [`literal_object`], through the
//!                  wrappers (`as`, `satisfies`, parentheses) the queries do not
//!                  need to know about
//!
//! A member's owners are found by following `within` -> literal links up to a
//! root. A literal reached any other way (a call argument, an array element, a
//! return value) has no name to own it: its members get no owners.

use std::collections::HashMap;

use tree_sitter::{QueryCursor, StreamingIterator};

use super::entities::strip_string_delimiters;
use super::node_text;
use crate::lang::{LanguagePack, ParserBackend};

/// One member of an object literal, found by its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralMember {
    /// The key as written, without quotes.
    pub name: String,
    /// The names that own it, outermost first. Empty for a member of a
    /// literal that is not bound to a name.
    pub owners: Vec<String>,
}

type ByteRange = (usize, usize);

/// A literal that has a name: bound by a declaration (`within` is `None`) or
/// the value of a key in an outer literal.
struct Scope {
    owner: String,
    within: Option<ByteRange>,
}

/// What the `@member.*` captures of a file say, gathered match by match.
///
/// One collector serves both users: [`literal_members`] runs the entity query
/// for nothing else (the SCIP import), and `extract_entities` feeds it the
/// matches it is walking anyway, to qualify the methods of a named literal.
#[derive(Default)]
pub(crate) struct MemberScan {
    scopes: HashMap<ByteRange, Scope>,
    /// (key position, key text, the literal it sits in)
    members: Vec<((u32, u32), String, ByteRange)>,
}

impl MemberScan {
    /// Take in one query match; a match with no `@member.*` capture adds nothing.
    pub(crate) fn add(
        &mut self,
        captures: &[tree_sitter::QueryCapture<'_>],
        names: &[&str],
        source: &[u8],
    ) {
        let (mut name, mut within, mut root, mut owner, mut value) = (None, None, None, None, None);
        for c in captures {
            match names[c.index as usize] {
                "member.name" => name = Some(c.node),
                "member.within" => within = Some(c.node),
                "member.root" => root = Some(c.node),
                "member.owner" => owner = Some(c.node),
                "member.value" => value = Some(c.node),
                _ => {}
            }
        }
        let range = |n: tree_sitter::Node| (n.start_byte(), n.end_byte());
        let text = |n| strip_string_delimiters(&node_text(n, source));
        if let (Some(name), Some(within)) = (name, within) {
            let p = name.start_position();
            self.members
                .push(((p.row as u32, p.column as u32), text(name), range(within)));
        } else if let (Some(root), Some(object)) = (root, value.and_then(literal_object)) {
            self.scopes.insert(
                range(object),
                Scope {
                    owner: text(root),
                    within: None,
                },
            );
        } else if let (Some(owner), Some(object), Some(within)) =
            (owner, value.and_then(literal_object), within)
        {
            self.scopes.insert(
                range(object),
                Scope {
                    owner: text(owner),
                    within: Some(range(within)),
                },
            );
        }
    }

    /// Every member, with the names that own it.
    pub(crate) fn resolve(self) -> HashMap<(u32, u32), LiteralMember> {
        let Self { scopes, members } = self;
        members
            .into_iter()
            .map(|(at, name, within)| {
                let mut owners = Vec::new();
                let mut rooted = false;
                let mut at_literal = Some(within);
                while let Some(scope) = at_literal.and_then(|r| scopes.get(&r)) {
                    owners.push(scope.owner.clone());
                    at_literal = scope.within;
                    if at_literal.is_none() {
                        rooted = true;
                    }
                }
                if !rooted {
                    owners.clear();
                }
                owners.reverse();
                (at, LiteralMember { name, owners })
            })
            .collect()
    }
}

/// The object literal `value` is, through the wrappers that keep it the same
/// value: `as`, `satisfies` (any number, in any order) and parentheses.
/// `{ .. } as const satisfies Record<..>` names the literal exactly as
/// `{ .. }` does. This is the only place that knows the wrapper kinds; the
/// queries capture the expression and leave the unwrapping to it.
fn literal_object(value: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut node = value;
    loop {
        match node.kind() {
            "object" => return Some(node),
            "as_expression" | "satisfies_expression" | "parenthesized_expression" => {
                node = node.named_child(0)?;
            }
            _ => return None,
        }
    }
}

/// Every member of every object literal in `source`, keyed by the 0-based
/// `(line, column)` of its key -- where SCIP puts the definition. Empty for a
/// pack that gives no `@member.*` captures, or one that is not tree-sitter.
pub fn literal_members(source: &[u8], pack: &LanguagePack) -> HashMap<(u32, u32), LiteralMember> {
    let ParserBackend::TreeSitter {
        grammar,
        entity_query,
        ..
    } = &pack.backend
    else {
        return HashMap::new();
    };
    let names = entity_query.capture_names();
    if !names.iter().any(|n| n.starts_with("member.")) {
        return HashMap::new();
    }
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(grammar).is_err() {
        return HashMap::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return HashMap::new();
    };
    let mut scan = MemberScan::default();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(entity_query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        scan.add(m.captures, names, source);
    }
    scan.resolve()
}
