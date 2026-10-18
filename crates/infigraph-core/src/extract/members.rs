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
//!   @member.scope  the object literal a root or an owner names
//!
//! A member's owners are found by following `within` -> `scope` links up to a
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
        let (mut name, mut within, mut root, mut owner, mut scope) = (None, None, None, None, None);
        for c in captures {
            match names[c.index as usize] {
                "member.name" => name = Some(c.node),
                "member.within" => within = Some(c.node),
                "member.root" => root = Some(c.node),
                "member.owner" => owner = Some(c.node),
                "member.scope" => scope = Some(c.node),
                _ => {}
            }
        }
        let range = |n: tree_sitter::Node| (n.start_byte(), n.end_byte());
        let text = |n| strip_string_delimiters(&node_text(n, source));
        if let (Some(name), Some(within)) = (name, within) {
            let p = name.start_position();
            self.members
                .push(((p.row as u32, p.column as u32), text(name), range(within)));
        } else if let (Some(scope), Some(root)) = (scope, root) {
            self.scopes.insert(
                range(scope),
                Scope {
                    owner: text(root),
                    within: None,
                },
            );
        } else if let (Some(scope), Some(owner), Some(within)) = (scope, owner, within) {
            self.scopes.insert(
                range(scope),
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
