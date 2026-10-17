//! Links from document sections to the code they describe.
//!
//! Two kinds, one table (`CodeLink` in `docs.kuzu`), one resolver:
//!
//! - **documents**: a *key heading* names a symbol id, and the section under
//!   it is that symbol's entry. The key is exact, so the link is authoritative.
//! - **mentions**: an inline code span in a section's text that looks like a
//!   qualified name or a `file::name`. Heuristic: it links only when it
//!   resolves to exactly one symbol, which the *reader* decides against the
//!   graph. Bare names never become mentions.
//!
//! The index stores what the text says and never consults the graph, so the
//! code graph and the document store keep separate writers and locks, and a
//! renamed symbol is a query result (a key that no longer resolves), not a
//! dangling reference.
//!
//! # Format
//!
//! A *key heading* is a Markdown heading of level 2 to 4 whose entire text is
//! one backtick span of the form `` `<symbol id>` `` where the id contains
//! `::` and no whitespace:
//!
//! ```text
//! ### `src/app.py::compute_total`
//! ```
//!
//! The entry runs to the next heading that is itself a key heading or has a
//! level at most its own, so `####` sub-entries under a `###` key belong to it
//! and key headings of one level never swallow each other. Fenced code is
//! skipped, so a heading inside a fence starts nothing. Nothing else is
//! configurable.

use std::sync::OnceLock;

use regex::Regex;

use crate::backend::DocBackend;
use crate::extract::{DocFormat, ExtractedDoc};

/// What the index stores about a document's links is a function of this
/// version: a document recorded under an older one is scanned again, even when
/// its content has not changed.
pub const CODE_LINKS_VERSION: i64 = 2;

/// A mention's excerpt is cut to this many characters around the span.
const EXCERPT_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Documents,
    Mentions,
    /// A heading that starts like a key but is not exactly one: it links
    /// nothing, and `doctor` lists it.
    NearMiss,
    /// A code fence that is opened and never closed. It is not treated as a
    /// fence (see [`prose_lines`]); `doctor` lists it so the author learns of it.
    UnclosedFence,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Documents => "documents",
            LinkKind::Mentions => "mentions",
            LinkKind::NearMiss => "near-miss",
            LinkKind::UnclosedFence => "unclosed-fence",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "documents" => Some(LinkKind::Documents),
            "mentions" => Some(LinkKind::Mentions),
            "near-miss" => Some(LinkKind::NearMiss),
            "unclosed-fence" => Some(LinkKind::UnclosedFence),
            _ => None,
        }
    }
}

/// One row of the `CodeLink` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeLinkRow {
    pub id: String,
    pub doc_file: String,
    pub kind: LinkKind,
    /// The key (documents), the span as written (mentions), or the heading
    /// text (near-miss).
    pub target: String,
    /// The text of the closest heading at or above the row.
    pub heading: String,
    /// 1-based lines of the document the row covers, inclusive. Blame reads
    /// these.
    pub line_start: usize,
    pub line_end: usize,
    /// The entry verbatim, `####` sub-entries included (documents); an
    /// excerpt around the span (mentions); empty (near-miss).
    pub text: String,
}

fn key_heading_text() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^`([^\s`]+::[^\s`]+)`$").unwrap())
}

fn heading_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(#{1,6})[ \t]+(.+?)[ \t]*$").unwrap())
}

fn code_span() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`([^`\n]+)`").unwrap())
}

/// `path/with.ext::name`: a file and a symbol's qualified name in it.
fn file_form() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:[\w@.$-]+/)*[\w@$-]+\.[A-Za-z]{1,5}::[\w.$<>:#@-]+$").unwrap()
    })
}

/// `Qualifier.name` or `Qualifier::name`, with no file.
fn qualified_form() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z_$][\w$]*(?:(?:\.|::)[A-Za-z_$][\w$]*)+$").unwrap())
}

/// Whether `span` is a `path.ext::name` mention: the form that names a file.
pub(crate) fn is_file_form(span: &str) -> bool {
    file_form().is_match(span)
}

/// A span that is a file name (`tasks.md`), which no symbol is.
fn names_a_file(span: &str) -> bool {
    const EXTENSIONS: [&str; 18] = [
        "md", "json", "toml", "yml", "yaml", "lock", "txt", "log", "sh", "css", "html", "ts",
        "tsx", "js", "mjs", "cjs", "py", "rs",
    ];
    !span.contains("::")
        && span
            .rsplit_once('.')
            .is_some_and(|(_, ext)| EXTENSIONS.contains(&ext))
}

/// Whether a span is shaped like something the reader could resolve.
fn is_mention_shaped(span: &str) -> bool {
    is_file_form(span) || (qualified_form().is_match(span) && !names_a_file(span))
}

struct Heading {
    line: usize,
    level: usize,
    text: String,
    key: Option<String>,
}

/// A run of backticks or tildes opening or closing a fenced code block: its
/// character, its length, and what follows it on the line.
fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
    let trimmed = line.trim_start();
    let c = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = trimmed.chars().take_while(|x| *x == c).count();
    let rest = &trimmed[n..];
    // A backtick fence's info string has no backtick: ```x``` is a code span.
    (n >= 3 && !(c == '`' && rest.contains('`'))).then_some((c, n, rest))
}

/// Whether `line` closes a fence opened with `n` of `c`.
fn closes_fence(line: &str, c: char, n: usize) -> bool {
    fence_marker(line).is_some_and(|(mc, mn, rest)| mc == c && mn >= n && rest.trim().is_empty())
}

/// What a Markdown document's fences leave: the lines outside them, and the
/// line of every fence that is opened and never closed.
struct Prose<'a> {
    /// Numbered from 1.
    lines: Vec<(usize, &'a str)>,
    unclosed: Vec<(usize, &'a str)>,
}

/// The lines of `text` outside fenced code.
///
/// A fence nothing closes is **not a fence**: its lines stay prose and it is
/// listed in `unclosed`. CommonMark would run it to the end of the file, but
/// then one stray line (a glossary page documenting its own format, with
/// fences inside a fence) hides every entry below it from the index, from
/// `doctor` and from the edit guard without a word. The price is that the
/// contents of a fence its author really never closed are read as prose.
fn prose_lines(text: &str) -> Prose<'_> {
    let all: Vec<&str> = text.lines().collect();
    let mut prose = Prose {
        lines: Vec::new(),
        unclosed: Vec::new(),
    };
    // The shortest opener of each character that found no closer: a longer
    // one finds none either, so the rest of the file is not scanned again.
    let mut no_closer: [Option<usize>; 2] = [None, None];
    let mut i = 0;
    while i < all.len() {
        let line = all[i];
        let Some((c, n, _)) = fence_marker(line) else {
            prose.lines.push((i + 1, line));
            i += 1;
            continue;
        };
        let slot = usize::from(c == '~');
        let closer = if no_closer[slot].is_some_and(|min| n >= min) {
            None
        } else {
            all[i + 1..]
                .iter()
                .position(|l| closes_fence(l, c, n))
                .map(|k| i + 1 + k)
        };
        match closer {
            Some(end) => i = end + 1,
            None => {
                no_closer[slot] = Some(no_closer[slot].map_or(n, |min| min.min(n)));
                prose.unclosed.push((i + 1, line.trim()));
                prose.lines.push((i + 1, line));
                i += 1;
            }
        }
    }
    prose
}

fn excerpt(line: &str, span: &str) -> String {
    let line = line.trim();
    if line.chars().count() <= EXCERPT_CHARS {
        return line.to_string();
    }
    let at = line.find(span).unwrap_or(0);
    let from = line[..at]
        .char_indices()
        .rev()
        .nth(EXCERPT_CHARS / 2)
        .map_or(0, |(i, _)| i);
    let cut: String = line[from..].chars().take(EXCERPT_CHARS).collect();
    format!("…{}…", cut.trim())
}

/// The headings of `text` outside fenced code, in order.
fn scan_headings(prose: &[(usize, &str)]) -> Vec<Heading> {
    let mut headings: Vec<Heading> = Vec::new();
    for (line, content) in prose {
        if let Some(m) = heading_line().captures(content) {
            let level = m[1].len();
            let text = m[2].to_string();
            let key = if (2..=4).contains(&level) {
                key_heading_text().captures(&text).map(|k| k[1].to_string())
            } else {
                None
            };
            headings.push(Heading {
                line: *line,
                level,
                text,
                key,
            });
        }
    }
    headings
}

/// The last line of the key section that starts at `headings[n]`: up to the
/// next key heading or a heading of its level or higher, without the blank
/// lines before it.
fn section_end(headings: &[Heading], n: usize, source_lines: &[&str]) -> usize {
    let heading = &headings[n];
    let end = headings[n + 1..]
        .iter()
        .find(|h| h.key.is_some() || h.level <= heading.level)
        .map_or(source_lines.len() + 1, |h| h.line);
    let mut last = end - 1;
    while last > heading.line && source_lines[last - 1].trim().is_empty() {
        last -= 1;
    }
    last
}

/// A heading inside an entry that is not itself a key; it rides along.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubHeading {
    pub line: usize,
    pub level: usize,
    pub text: String,
}

/// One entry of a Markdown document, as its lines lie now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    /// The key heading's level (2-4).
    pub level: usize,
    /// 1-based, inclusive: the key heading's line to the entry's last
    /// non-blank line.
    pub start: usize,
    pub end: usize,
    /// The ride-along headings inside the entry, in order.
    pub subs: Vec<SubHeading>,
}

/// The entries of a Markdown document, in order. The same scan `extract_markdown`
/// stores, so an edit made by line range and the index agree on where an entry
/// is.
pub fn entries(text: &str) -> Vec<Entry> {
    let prose = prose_lines(text);
    let headings = scan_headings(&prose.lines);
    let source_lines: Vec<&str> = text.lines().collect();
    headings
        .iter()
        .enumerate()
        .filter_map(|(n, heading)| {
            let key = heading.key.clone()?;
            let end = section_end(&headings, n, &source_lines);
            let subs = headings[n + 1..]
                .iter()
                .take_while(|h| h.line <= end)
                .map(|h| SubHeading {
                    line: h.line,
                    level: h.level,
                    text: h.text.clone(),
                })
                .collect();
            Some(Entry {
                key,
                level: heading.level,
                start: heading.line,
                end,
                subs,
            })
        })
        .collect()
}

/// Every link a Markdown document carries.
pub fn extract_markdown(doc_file: &str, text: &str) -> Vec<CodeLinkRow> {
    let prose = prose_lines(text);
    let headings = scan_headings(&prose.lines);

    let mut rows: Vec<CodeLinkRow> = Vec::new();
    let next_id = |rows: &Vec<CodeLinkRow>| format!("{doc_file}#{}", rows.len());
    let source_lines: Vec<&str> = text.lines().collect();

    for (n, heading) in headings.iter().enumerate() {
        let starts_like_key = (2..=4).contains(&heading.level)
            && heading.text.starts_with('`')
            && heading.text.contains("::");
        match &heading.key {
            Some(key) => {
                let last = section_end(&headings, n, &source_lines);
                rows.push(CodeLinkRow {
                    id: next_id(&rows),
                    doc_file: doc_file.to_string(),
                    kind: LinkKind::Documents,
                    target: key.clone(),
                    heading: heading.text.clone(),
                    line_start: heading.line,
                    line_end: last,
                    text: source_lines[heading.line - 1..last].join("\n"),
                });
            }
            None if starts_like_key => rows.push(CodeLinkRow {
                id: next_id(&rows),
                doc_file: doc_file.to_string(),
                kind: LinkKind::NearMiss,
                target: heading.text.clone(),
                heading: heading.text.clone(),
                line_start: heading.line,
                line_end: heading.line,
                text: String::new(),
            }),
            None => {}
        }
    }

    // Mentions: one row per span per section, at its first line.
    let mut section = String::new();
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    let heading_at: std::collections::HashMap<usize, &Heading> =
        headings.iter().map(|h| (h.line, h)).collect();
    for (line, content) in &prose.lines {
        if let Some(h) = heading_at.get(line) {
            section = h.text.clone();
            if h.key.is_some() {
                continue; // the key itself is a documents row, not a mention
            }
        }
        for m in code_span().captures_iter(content) {
            let span = m[1].trim();
            if !is_mention_shaped(span) || !seen.insert((section.clone(), span.to_string())) {
                continue;
            }
            rows.push(CodeLinkRow {
                id: next_id(&rows),
                doc_file: doc_file.to_string(),
                kind: LinkKind::Mentions,
                target: span.to_string(),
                heading: section.clone(),
                line_start: *line,
                line_end: *line,
                text: excerpt(content, span),
            });
        }
    }
    for (line, opener) in &prose.unclosed {
        rows.push(CodeLinkRow {
            id: next_id(&rows),
            doc_file: doc_file.to_string(),
            kind: LinkKind::UnclosedFence,
            target: opener.to_string(),
            heading: String::new(),
            line_start: *line,
            line_end: *line,
            text: String::new(),
        });
    }
    rows
}

/// Every link a document carries; only Markdown carries any.
pub fn extract(doc: &ExtractedDoc) -> Vec<CodeLinkRow> {
    if doc.format == DocFormat::Markdown {
        extract_markdown(&doc.file, &doc.text)
    } else {
        Vec::new()
    }
}

/// Replaces `doc`'s links in `store`. A failure is a warning: the document is
/// left without a scan row, so it is scanned again by the next index run and
/// readers report it as not scanned.
pub fn record(store: &dyn DocBackend, doc: &ExtractedDoc) {
    if let Err(e) = store.replace_code_links(&doc.file, &extract(doc)) {
        eprintln!("warn: code links of {} not recorded: {e:#}", doc.file);
    }
}

/// [`record`] for a document already in the store, read from `path`. Markdown
/// is read as text; any other format has no links and gets only its scan row.
pub fn record_stored(store: &dyn DocBackend, path: &std::path::Path, doc_id: &str) {
    let is_markdown = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_lowercase().as_str(), "md" | "markdown"));
    let text = if is_markdown {
        match std::fs::read(path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => return, // gone from disk: the stale-document prune's business
        }
    } else {
        String::new()
    };
    record(
        store,
        &ExtractedDoc {
            file: doc_id.to_string(),
            title: None,
            content_hash: String::new(),
            format: if is_markdown {
                DocFormat::Markdown
            } else {
                DocFormat::PlainText
            },
            text,
            page_count: None,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<CodeLinkRow> {
        extract_markdown("docs/g.md", text)
    }

    fn documents(text: &str) -> Vec<CodeLinkRow> {
        rows(text)
            .into_iter()
            .filter(|r| r.kind == LinkKind::Documents)
            .collect()
    }

    #[test]
    fn a_key_heading_names_a_symbol_and_owns_its_sub_entries() {
        let got = documents(
            "# Title\n\n### `a/b.ts::f`\n\nWhy f.\n\n#### body\n\nInside f.\n\n### `a/b.ts::g`\n\nWhy g.\n",
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].target, "a/b.ts::f");
        assert_eq!((got[0].line_start, got[0].line_end), (3, 9));
        assert!(got[0].text.contains("Inside f."), "{:?}", got[0].text);
        assert!(!got[0].text.contains("Why g."));
        assert_eq!(got[1].target, "a/b.ts::g");
        assert_eq!((got[1].line_start, got[1].line_end), (11, 13));
    }

    #[test]
    fn a_key_may_sit_at_level_2_3_or_4_and_same_level_keys_do_not_swallow_each_other() {
        let got = documents(
            "## `a.ts::two`\n\ntwo\n\n### `a.ts::three`\n\nthree\n\n#### `a.ts::four`\n\nfour\n\n#### `a.ts::four_b`\n\nfour b\n\n## `a.ts::two_b`\n\ntwo b\n",
        );
        let by_target: std::collections::HashMap<_, _> =
            got.iter().map(|r| (r.target.as_str(), r)).collect();
        assert_eq!(got.len(), 5, "{got:#?}");
        // A key heading ends the entry above it, whatever its level.
        assert_eq!(by_target["a.ts::two"].text, "## `a.ts::two`\n\ntwo");
        assert_eq!(by_target["a.ts::three"].text, "### `a.ts::three`\n\nthree");
        assert_eq!(by_target["a.ts::four"].text, "#### `a.ts::four`\n\nfour");
        assert_eq!(
            by_target["a.ts::four_b"].text,
            "#### `a.ts::four_b`\n\nfour b"
        );
        assert_eq!(by_target["a.ts::two_b"].text, "## `a.ts::two_b`\n\ntwo b");
    }

    #[test]
    fn a_plain_sub_heading_rides_along_with_its_key() {
        let got =
            documents("### `a.ts::f`\n\nWhy.\n\n#### notes\n\nMore.\n\n## Other\n\nUnrelated.\n");
        assert_eq!(got.len(), 1);
        assert!(got[0].text.contains("More."));
        assert!(!got[0].text.contains("Unrelated."));
    }

    #[test]
    fn levels_1_and_5_and_6_are_not_keys() {
        assert!(documents("# `a.ts::f`\n\nx\n").is_empty());
        assert!(documents("##### `a.ts::f`\n\nx\n").is_empty());
        assert!(documents("###### `a.ts::f`\n\nx\n").is_empty());
    }

    #[test]
    fn a_heading_inside_a_fence_starts_nothing_and_ends_nothing() {
        let got = documents(
            "### `a.ts::f`\n\n```md\n### `a.ts::fake`\n## stop\n```\n\nstill f\n\n~~~\n### `b.ts::tilde`\n~~~\n",
        );
        assert_eq!(got.len(), 1, "{got:#?}");
        assert_eq!(got[0].target, "a.ts::f");
        assert!(got[0].text.contains("still f"));
    }

    /// The shape of a glossary README that documents its own format: an outer
    /// fence holding inner ```text fences. The first bare fence line closes the
    /// outer one, so the last line below opens a fence nothing ever closes.
    const NESTED_FENCES: &str = "# Glossary\n\nInside a file:\n\n```\n### `a.ts::fenced`\n\n```text\n<comment>\n```\n\n#### body\n\n```text\n<inner>\n```\n```\n\nAfter.\n\n### `a.ts::real`\n\nWhy real.\n";

    #[test]
    fn a_fence_nothing_closes_swallows_nothing() {
        let got = documents(NESTED_FENCES);
        assert_eq!(
            got.iter().map(|r| r.target.as_str()).collect::<Vec<_>>(),
            ["a.ts::real"],
            "the example inside the fence is not a key, the entry after the stray fence is"
        );
        assert_eq!(got[0].line_start, 21);
        let entries = entries(NESTED_FENCES);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            (entries[0].key.as_str(), entries[0].start),
            ("a.ts::real", 21)
        );
    }

    #[test]
    fn an_unclosed_fence_is_reported_at_its_opening_line() {
        let open: Vec<_> = rows(NESTED_FENCES)
            .into_iter()
            .filter(|r| r.kind == LinkKind::UnclosedFence)
            .collect();
        assert_eq!(open.len(), 1, "{open:#?}");
        assert_eq!((open[0].line_start, open[0].target.as_str()), (17, "```"));
        let tilde: Vec<_> = rows("text\n\n~~~ps\n### `a.ts::f`\n\nwhy\n")
            .into_iter()
            .filter(|r| r.kind == LinkKind::UnclosedFence)
            .collect();
        assert_eq!(tilde.len(), 1);
        assert_eq!(tilde[0].line_start, 3);
        // A closed fence reports nothing.
        assert!(rows("```\ncode\n```\n")
            .iter()
            .all(|r| r.kind != LinkKind::UnclosedFence));
    }

    #[test]
    fn a_line_that_starts_with_an_inline_span_is_not_a_fence() {
        let text = "```x``` is a span, not a fence.\n\n### `a.ts::f`\n\nwhy\n";
        assert_eq!(documents(text).len(), 1);
        assert!(rows(text).iter().all(|r| r.kind != LinkKind::UnclosedFence));
    }

    #[test]
    fn a_longer_closer_closes_and_a_shorter_one_does_not() {
        let got = documents("````\n```\n### `a.ts::fake`\n```\n````\n\n### `a.ts::f`\n\nwhy\n");
        assert_eq!(got.len(), 1, "{got:#?}");
        assert_eq!(got[0].target, "a.ts::f");
    }

    #[test]
    fn a_heading_that_is_not_exactly_one_key_span_links_nothing_and_is_a_near_miss() {
        let all = rows(
            "### `a.ts::f` — the first\n\nx\n\n### `a.ts`\n\nx\n\n### Not code\n\nx\n\n### `a b::c`\n\nx\n",
        );
        assert!(
            all.iter().all(|r| r.kind != LinkKind::Documents),
            "{all:#?}"
        );
        let near: Vec<_> = all
            .iter()
            .filter(|r| r.kind == LinkKind::NearMiss)
            .map(|r| r.target.as_str())
            .collect();
        assert_eq!(near, ["`a.ts::f` — the first", "`a b::c`"]);
    }

    #[test]
    fn the_last_entry_runs_to_the_end_and_loses_trailing_blank_lines() {
        let got = documents("### `a.ts::f`\n\nlast words\n\n\n");
        assert_eq!(got[0].text, "### `a.ts::f`\n\nlast words");
        assert_eq!(got[0].line_end, 3);
    }

    #[test]
    fn mentions_are_qualified_or_file_forms_and_never_bare_names() {
        let got: Vec<_> = rows(
            "Call `routes.ts::admittedKinds` or `Engine::parse`, see `Foo.bar` and `bare` and `tasks.md` and `a b`.\n",
        )
        .into_iter()
        .filter(|r| r.kind == LinkKind::Mentions)
        .map(|r| r.target)
        .collect();
        assert_eq!(
            got,
            ["routes.ts::admittedKinds", "Engine::parse", "Foo.bar"]
        );
    }

    #[test]
    fn a_mention_keeps_its_line_section_and_an_excerpt_and_is_listed_once_per_section() {
        let got: Vec<_> =
            rows("# One\n\nUse `A.b` here.\nAnd `A.b` again.\n\n# Two\n\n`A.b` too.\n")
                .into_iter()
                .filter(|r| r.kind == LinkKind::Mentions)
                .collect();
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].heading.as_str(), got[0].line_start), ("One", 3));
        assert_eq!(got[0].text, "Use `A.b` here.");
        assert_eq!((got[1].heading.as_str(), got[1].line_start), ("Two", 8));
    }

    #[test]
    fn a_span_inside_a_fence_or_a_key_heading_is_not_a_mention() {
        let got = rows("### `a.ts::f`\n\n```\n`Foo.bar`\n```\n");
        assert!(got.iter().all(|r| r.kind != LinkKind::Mentions), "{got:#?}");
    }

    #[test]
    fn only_markdown_carries_links() {
        let mut doc = ExtractedDoc {
            file: "x.txt".into(),
            title: None,
            content_hash: String::new(),
            format: DocFormat::PlainText,
            text: "### `a.ts::f`\n\nx\n".into(),
            page_count: None,
        };
        assert!(extract(&doc).is_empty());
        doc.format = DocFormat::Markdown;
        assert_eq!(extract(&doc).len(), 1);
    }

    #[test]
    fn a_long_line_is_cut_to_an_excerpt_around_the_span() {
        let line = format!("{} `Foo.bar` {}", "x".repeat(400), "y".repeat(400));
        let cut = excerpt(&line, "`Foo.bar`");
        assert!(cut.contains("Foo.bar"));
        assert!(cut.chars().count() <= EXCERPT_CHARS + 2, "{}", cut.len());
    }
}
