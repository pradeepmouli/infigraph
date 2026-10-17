//! What the document store knows about one symbol: its glossary entry, its
//! file's notes, and the documents that mention it.
//!
//! The one lookup both `get_doc_context` and any later renderer call. The
//! documents side stores what the text says (`code_links`); this is the
//! reader that decides what that means for a symbol, against the graph it is
//! handed:
//!
//! - an entry is an exact key match on the symbol id, so it needs no graph;
//! - a file's notes are the entry keyed `<file>::module`, one more target in
//!   the same lookup;
//! - a mention counts only when the string it was written as resolves to
//!   exactly this one symbol, which is a count asked of the graph at read time.
//!   A stale or ambiguous mention is not a link.

use std::path::Path;

use infigraph_core::degraded::{live, DegradedMode, DOC_LINKS_UNSCANNED, DOC_SYNC_UNKNOWN};
use infigraph_core::docs_switch::DocsNotIndexed;
use infigraph_core::escape_str;
use infigraph_core::git_blame::{self, BlameError, GIT_TIMEOUT};
use infigraph_core::graph::GraphBackend;

use crate::ack::Acks;
use crate::backend::DocBackend;
use crate::code_links::{is_file_form, CodeLinkRow, LinkKind, CODE_LINKS_VERSION};
use crate::stale_docs::verdict;
use crate::DocIndex;

/// An entry longer than this many characters is cut, and says so.
pub const ENTRY_CAP_CHARS: usize = 4_000;

/// At most this many mentions are listed.
pub const MENTION_CAP: usize = 5;

/// At most this many distinct mention strings are checked against the graph
/// per lookup: each check is a scan.
const VERIFY_CAP: usize = 20;

/// A glossary entry, or the notes on a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedEntry {
    pub doc_file: String,
    pub line_start: usize,
    pub line_end: usize,
    /// The entry without its key heading, cut to [`ENTRY_CAP_CHARS`].
    pub text: String,
    pub truncated: bool,
    /// "possibly stale: code changed ..., entry last touched ...", when the
    /// symbol changed after the entry did. Set for a symbol's own entry.
    pub stale: Option<String>,
}

impl LinkedEntry {
    fn from_row(row: &CodeLinkRow) -> Self {
        let body = row
            .text
            .split_once('\n')
            .map_or("", |(_, rest)| rest)
            .trim_matches('\n');
        let (text, truncated) = if body.chars().count() > ENTRY_CAP_CHARS {
            let cut = body.floor_char_boundary(
                body.char_indices()
                    .nth(ENTRY_CAP_CHARS)
                    .map_or(body.len(), |(i, _)| i),
            );
            (body[..cut].trim_end().to_string(), true)
        } else {
            (body.to_string(), false)
        };
        Self {
            doc_file: row.doc_file.clone(),
            line_start: row.line_start,
            line_end: row.line_end,
            text,
            truncated,
            stale: None,
        }
    }
}

/// A document that names the symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub doc_file: String,
    pub heading: String,
    pub line: usize,
    pub excerpt: String,
}

/// Everything linked to a symbol. `Default` is "nothing": a project that has
/// not opted in to documents, or a symbol no document names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkedDocs {
    pub entries: Vec<LinkedEntry>,
    pub file_notes: Vec<LinkedEntry>,
    /// Up to [`MENTION_CAP`] of `mentions_total`.
    pub mentions: Vec<Mention>,
    pub mentions_total: usize,
    /// `(documents not scanned, documents)`, when any was not.
    pub unscanned: Option<(usize, usize)>,
    /// Why the document store could not be read, when it could not.
    pub unavailable: Option<String>,
    /// Why git could not judge whether the entry is current, when it could
    /// not.
    pub sync_unknown: Option<String>,
}

/// The strings a document could have used to name `symbol_id` in a code span.
///
/// An id is `file::qualified::name`; a document abbreviates the file to any
/// path suffix and may spell the qualifier with `.`. A bare name is never a
/// candidate, and a `name#N` symbol (one of several with the same name in its
/// file) has no stable spelling to mention.
pub(crate) fn mention_candidates(symbol_id: &str) -> Vec<String> {
    let mut out = vec![symbol_id.to_string()];
    let Some((file, qualified)) = symbol_id.split_once("::") else {
        return out;
    };
    let is_numbered = qualified
        .rsplit_once('#')
        .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if is_numbered {
        return out;
    }
    let mut spellings = vec![qualified.to_string()];
    let dotted = qualified.replace("::", ".");
    if dotted != qualified {
        spellings.push(dotted);
    }
    let parts: Vec<&str> = file.split('/').collect();
    for start in 0..parts.len() {
        let suffix = parts[start..].join("/");
        for spelling in &spellings {
            out.push(format!("{suffix}::{spelling}"));
        }
    }
    if qualified.contains("::") {
        out.extend(spellings);
    }
    out.sort();
    out.dedup();
    out
}

/// The ids a mention written as `target` can mean, at most two (a third would
/// change nothing: more than one is already ambiguous).
fn ids_named_by(backend: &dyn GraphBackend, target: &str) -> Vec<String> {
    let cypher = match target.split_once("::") {
        Some((file, qualified)) if is_file_form(target) => {
            let id = escape_str(&format!("{file}::{}", qualified.replace('.', "::")));
            format!(
                "MATCH (s:Symbol) WHERE s.id = '{id}' OR s.id ENDS WITH '/{id}' \
                 RETURN s.id LIMIT 2"
            )
        }
        _ => {
            let tail = escape_str(&target.replace('.', "::"));
            format!("MATCH (s:Symbol) WHERE s.id ENDS WITH '::{tail}' RETURN s.id LIMIT 2")
        }
    };
    backend
        .raw_query(&cypher)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .collect()
}

/// Everything the document store links to `symbol_id` (a symbol of `file`),
/// read against `backend`.
///
/// `lines` is the symbol's first and last line in `file`: the code half of
/// the staleness question, which git answers for just those lines.
pub fn lookup(
    root: &Path,
    backend: &dyn GraphBackend,
    symbol_id: &str,
    file: &str,
    lines: (usize, usize),
) -> LinkedDocs {
    let index = match DocIndex::open_existing(root) {
        Ok(index) => index,
        Err(e) if e.is::<DocsNotIndexed>() => return LinkedDocs::default(),
        Err(e) => {
            return LinkedDocs {
                unavailable: Some(format!("{e:#}")),
                ..LinkedDocs::default()
            }
        }
    };
    let Some(store) = index.store() else {
        return LinkedDocs::default();
    };
    let mut found = LinkedDocs::default();

    let module_key = format!("{file}::module");
    let mut keys = vec![symbol_id.to_string()];
    if module_key != symbol_id {
        keys.push(module_key.clone());
    }
    match store.code_links_for(LinkKind::Documents, &keys) {
        Ok(rows) => {
            for row in &rows {
                let entry = LinkedEntry::from_row(row);
                if row.target == symbol_id {
                    found.entries.push(entry);
                } else {
                    found.file_notes.push(entry);
                }
            }
        }
        Err(e) => found.unavailable = Some(format!("{e:#}")),
    }

    // The mentions of the symbol, each checked to mean this symbol alone.
    let candidates = mention_candidates(symbol_id);
    if let Ok(rows) = store.code_links_for(LinkKind::Mentions, &candidates) {
        let mut verdict: std::collections::HashMap<String, bool> = Default::default();
        let mut mentions = Vec::new();
        for row in rows {
            let inside_own_entry = found.entries.iter().any(|e| {
                e.doc_file == row.doc_file && (e.line_start..=e.line_end).contains(&row.line_start)
            });
            if inside_own_entry {
                continue;
            }
            let means_it = match verdict.get(&row.target) {
                Some(known) => *known,
                None if verdict.len() >= VERIFY_CAP => false,
                None => {
                    let unique = ids_named_by(backend, &row.target) == [symbol_id];
                    verdict.insert(row.target.clone(), unique);
                    unique
                }
            };
            if means_it {
                mentions.push(Mention {
                    doc_file: row.doc_file,
                    heading: row.heading,
                    line: row.line_start,
                    excerpt: row.text,
                });
            }
        }
        found.mentions_total = mentions.len();
        mentions.truncate(MENTION_CAP);
        found.mentions = mentions;
    }

    judge_entries(root, store, symbol_id, file, lines, &mut found);

    found.unscanned = unscanned_documents(store);
    match found.unscanned {
        Some((unscanned, total)) => {
            live::set(root, DegradedMode::DocLinksUnscanned { unscanned, total })
        }
        None => live::clear(root, DOC_LINKS_UNSCANNED),
    }
    found
}

/// Marks each of the symbol's entries stale when the symbol's lines changed
/// after the entry's did. Git is asked for those two ranges only. A failure
/// is the degraded mode `doc-sync-unknown` for this call: no entry is called
/// current or stale, and nothing waits longer than the git timeout.
fn judge_entries(
    root: &Path,
    store: &dyn DocBackend,
    symbol_id: &str,
    file: &str,
    lines: (usize, usize),
    found: &mut LinkedDocs,
) {
    if found.entries.is_empty() {
        return;
    }
    // A review that cannot be read leaves the entry judged by its history.
    let acks = Acks::of(store, &[symbol_id.to_string()]).unwrap_or_default();
    let judge = |entry: &LinkedEntry| -> Result<Option<String>, BlameError> {
        let code = git_blame::blame_ranges_cached(root, file, &[lines], GIT_TIMEOUT)?;
        let doc = git_blame::blame_ranges_cached(
            root,
            &entry.doc_file,
            &[(entry.line_start, entry.line_end)],
            GIT_TIMEOUT,
        )?;
        Ok(verdict(symbol_id, &code[0], &doc[0], &acks).state.label())
    };
    let mut failure = None;
    for entry in &mut found.entries {
        match judge(entry) {
            Ok(label) => entry.stale = label,
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        }
    }
    match &failure {
        Some(reason) => live::set(
            root,
            DegradedMode::DocSyncUnknown {
                reason: reason.clone(),
            },
        ),
        None => live::clear(root, DOC_SYNC_UNKNOWN),
    }
    found.sync_unknown = failure;
}

/// `(unscanned, total)` over the stored local documents, when any was not
/// scanned under the current version.
fn unscanned_documents(store: &dyn crate::backend::DocBackend) -> Option<(usize, usize)> {
    let stored = store.get_doc_hashes().ok()?;
    let scans = store.code_link_scans().ok()?;
    let local: Vec<&String> = stored
        .keys()
        .filter(|id| !id.is_empty() && !id.contains("://"))
        .collect();
    let unscanned = local
        .iter()
        .filter(|id| scans.get(**id).copied().unwrap_or(0) < CODE_LINKS_VERSION)
        .count();
    (unscanned > 0).then_some((unscanned, local.len()))
}

fn where_is(doc_file: &str, start: usize, end: usize) -> String {
    format!("{doc_file}:{start}-{end}")
}

impl LinkedDocs {
    /// The entry and file-notes blocks, each headed by where it came from;
    /// empty when there are none.
    pub fn render_entries(&self) -> String {
        let mut out = String::new();
        for (title, entries) in [
            ("Glossary", &self.entries),
            ("File-level notes", &self.file_notes),
        ] {
            for e in entries {
                let stale = e
                    .stale
                    .as_ref()
                    .map_or(String::new(), |s| format!(" -- {s}"));
                out.push_str(&format!(
                    "\n{title} ({}){stale}:\n{}\n",
                    where_is(&e.doc_file, e.line_start, e.line_end),
                    e.text
                ));
                if e.truncated {
                    out.push_str(&format!(
                        "… (cut at {ENTRY_CAP_CHARS} characters; the whole entry is at {})\n",
                        where_is(&e.doc_file, e.line_start, e.line_end)
                    ));
                }
            }
        }
        out
    }

    /// The mentions block, labelled as heuristic, and the notes about the
    /// document store itself; empty when there is nothing to say.
    pub fn render_mentions(&self) -> String {
        let mut out = String::new();
        if !self.mentions.is_empty() {
            out.push_str(&format!(
                "\nMentioned in (heuristic: a code span that names only this symbol; {} of {}):\n",
                self.mentions.len(),
                self.mentions_total
            ));
            for m in &self.mentions {
                let section = if m.heading.is_empty() {
                    String::new()
                } else {
                    format!(" § {}", m.heading)
                };
                out.push_str(&format!(
                    "  {}:{}{}\n    {}\n",
                    m.doc_file, m.line, section, m.excerpt
                ));
            }
        }
        if let Some((unscanned, total)) = self.unscanned {
            out.push_str(&format!(
                "\nDoc links: {unscanned} of {total} documents are not scanned yet; this may be \
                 incomplete (run `infigraph index-docs`)\n"
            ));
        }
        if let Some(reason) = &self.sync_unknown {
            out.push_str(&format!(
                "\nDoc/code sync not judged: {reason} (entries are shown without a staleness label)\n"
            ));
        }
        if let Some(reason) = &self.unavailable {
            out.push_str(&format!("\nLinked docs unavailable: {reason}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_top_level_symbol_is_mentioned_by_its_id_or_any_path_suffix_of_it() {
        let got = mention_candidates("packages/codegen/src/routes.ts::admittedKinds");
        for want in [
            "packages/codegen/src/routes.ts::admittedKinds",
            "codegen/src/routes.ts::admittedKinds",
            "src/routes.ts::admittedKinds",
            "routes.ts::admittedKinds",
        ] {
            assert!(got.contains(&want.to_string()), "{want} not in {got:?}");
        }
        assert!(
            !got.contains(&"admittedKinds".to_string()),
            "a bare name is never a mention: {got:?}"
        );
    }

    #[test]
    fn a_member_is_also_mentioned_by_its_qualifier_in_either_spelling() {
        let got = mention_candidates("src/node-map.ts::AssembledEnum::storage");
        for want in [
            "AssembledEnum::storage",
            "AssembledEnum.storage",
            "node-map.ts::AssembledEnum.storage",
            "src/node-map.ts::AssembledEnum::storage",
        ] {
            assert!(got.contains(&want.to_string()), "{want} not in {got:?}");
        }
    }

    #[test]
    fn a_numbered_duplicate_has_no_mention_spelling() {
        assert_eq!(
            mention_candidates("types.ts::rules#2"),
            ["types.ts::rules#2"]
        );
    }

    #[test]
    fn an_entry_is_shown_without_its_key_heading_and_is_cut_when_long() {
        let row = CodeLinkRow {
            id: "d#0".into(),
            doc_file: "d.md".into(),
            kind: LinkKind::Documents,
            target: "a.ts::f".into(),
            heading: "`a.ts::f`".into(),
            line_start: 1,
            line_end: 3,
            text: format!("### `a.ts::f`\n\n{}", "w ".repeat(ENTRY_CAP_CHARS)),
        };
        let entry = LinkedEntry::from_row(&row);
        assert!(entry.truncated);
        assert!(entry.text.starts_with("w w"));
        assert!(entry.text.chars().count() <= ENTRY_CAP_CHARS);

        let short = LinkedEntry::from_row(&CodeLinkRow {
            text: "### `a.ts::f`\n\nWhy.".into(),
            ..row
        });
        assert_eq!((short.text.as_str(), short.truncated), ("Why.", false));
    }
}
