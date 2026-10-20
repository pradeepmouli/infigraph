//! Which symbols a diff adds that have no glossary entry, in the directories
//! that already keep a glossary: the counterpart of [`crate::stale_docs`],
//! which finds entries whose code moved. This catches the entry that was never
//! written, most often because the code was created in the commit that should
//! have carried it.
//!
//! This is the one rule. The `missing_docs` tool and the `missing-docs`
//! command parse arguments and print [`Report::render`]; none keeps its own.
//! Read-only, like `stale_docs`: it says what to document and hands over a
//! `doc_entry create` call to run (a dry run until `apply`), but never writes
//! prose.
//!
//! The diff is the base's merge-base against the working tree
//! (`git_blame::diff_hunks`), because the graph indexes the working tree and
//! new-side lines of any other pair would not match it. "Which glossary" is
//! `doc_entry::glossary_for`, the rule `create` itself writes by, so a listed
//! hint cannot disagree with where the entry goes. The glossary file wins over
//! the stored index: a candidate document is parsed and a symbol it already
//! has an entry for is not listed, whatever the index (not yet refreshed) says.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use infigraph_core::docs_switch;
use infigraph_core::git_blame::{self, BlameError, Hunk};
use infigraph_core::path_scope::PathScope;

use crate::code_links::{entries, CodeLinkRow, LinkKind};
use crate::doc_entry::{glossary_for, Glossary};
use crate::link_check::{self, FileSymbol};

/// Symbols listed when the caller names no limit.
pub const DEFAULT_LIMIT: usize = 50;

/// A declaration shorter than this many lines is a stub or a getter.
pub const DEFAULT_MIN_LINES: usize = 3;

/// One git question may take this long (the diff of a large branch is the
/// slowest).
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// Kinds a glossary describes. Variables, constants, fields, sections and
/// modules are not declarations anyone writes an entry for; tests are skipped
/// by kind and by path.
const DECLARATIONS: &[&str] = &[
    "function",
    "method",
    "class",
    "struct",
    "interface",
    "trait",
    "enum",
];

/// What to list.
#[derive(Debug, Clone)]
pub struct Options {
    /// Only files under it.
    pub scope: PathScope,
    /// The ref the diff starts from; `None` takes the default (see
    /// [`resolve_base`]).
    pub base: Option<String>,
    /// Also symbols the diff changed, not only the ones it adds.
    pub include_changed: bool,
    /// Also symbols that are not public.
    pub all_visibility: bool,
    /// Declarations shorter than this are skipped.
    pub min_lines: usize,
    pub limit: usize,
}

impl Options {
    /// The tool's and the command's arguments, validated once for both.
    /// `visibility` is `public` (the default) or `all`.
    pub fn from_args(
        scope: PathScope,
        base: Option<String>,
        include_changed: bool,
        visibility: Option<&str>,
        min_lines: Option<usize>,
        limit: Option<usize>,
    ) -> Result<Self, String> {
        let all_visibility = match visibility {
            None | Some("public") => false,
            Some("all") => true,
            Some(other) => return Err(format!("visibility is `public` or `all`, not `{other}`")),
        };
        Ok(Self {
            scope,
            base,
            include_changed,
            all_visibility,
            min_lines: min_lines.unwrap_or(DEFAULT_MIN_LINES),
            limit: limit.unwrap_or(DEFAULT_LIMIT).max(1),
        })
    }
}

/// The ref the diff starts from and how it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    /// What was named: the `base` argument, the upstream, `origin/HEAD`, ...
    pub name: String,
    pub via: &'static str,
    /// The merge-base of that ref and HEAD, the commit the diff starts from.
    pub commit: String,
}

/// The ref to diff from, in this order: the `base` argument, the current
/// branch's upstream, `origin/HEAD`, `main`, `master`. The first that exists
/// and shares history with HEAD wins; an explicit `base` that does not is an
/// error rather than a fall-through. `base=HEAD` is uncommitted edits only.
pub fn resolve_base(root: &Path, explicit: Option<&str>) -> Result<Base, String> {
    let ask = |args: &[&str]| git_blame::git(root, args, GIT_TIMEOUT);
    let merge_base_of = |name: &str| -> Result<String, String> {
        let spec = format!("{name}^{{commit}}");
        ask(&["rev-parse", "--verify", "--quiet", &spec])
            .map_err(|_| format!("`{name}` is not a commit"))?;
        ask(&["merge-base", "HEAD", name])
            .map(|out| out.trim().to_string())
            .map_err(|_| format!("`{name}` shares no history with HEAD"))
    };
    if ask(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).is_err() {
        return Err("this repository has no commit yet, so there is nothing to diff from".into());
    }
    if let Some(name) = explicit {
        return merge_base_of(name)
            .map(|commit| Base {
                name: name.to_string(),
                via: "the `base` argument",
                commit,
            })
            .map_err(|why| format!("base: {why}"));
    }
    let mut tried: Vec<String> = Vec::new();
    let upstream = ask(&[
        "rev-parse",
        "--abbrev-ref",
        "--symbolic-full-name",
        "@{upstream}",
    ])
    .ok()
    .map(|out| out.trim().to_string())
    .filter(|name| !name.is_empty());
    let candidates = upstream
        .map(|name| (name, "the branch's upstream"))
        .into_iter()
        .chain(
            [
                ("origin/HEAD", "origin/HEAD"),
                ("main", "main"),
                ("master", "master"),
            ]
            .map(|(name, via)| (name.to_string(), via)),
        );
    for (name, via) in candidates {
        match merge_base_of(&name) {
            Ok(commit) => return Ok(Base { name, via, commit }),
            Err(why) => tried.push(why),
        }
    }
    Err(format!(
        "no base to diff from: the branch has no upstream and none of origin/HEAD, main, master \
         works ({}); pass `base`",
        tried.join("; ")
    ))
}

/// How the diff touches a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// Every line of it is new in the diff (a moved function reads as added).
    Added,
    Changed,
}

/// A symbol with no glossary entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub symbol: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub change: Change,
    /// Where `doc_entry create` would put the entry.
    pub doc_file: String,
}

/// Symbols the rule looked at and left out, by the first reason that applied,
/// so `considered == listed + the sum of these`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Skipped {
    pub not_a_declaration: usize,
    /// Changed, not added, and `include_changed` is off.
    pub changed: usize,
    pub documented: usize,
    pub test: usize,
    pub generated: usize,
    pub trivial: usize,
    pub not_public: usize,
    /// Of `not_public`: the extractor stored no visibility, which is read as
    /// not public (a language with real modifiers leaves an unmarked
    /// declaration private).
    pub unknown_visibility: usize,
}

impl Skipped {
    fn total(&self) -> usize {
        self.not_a_declaration
            + self.changed
            + self.documented
            + self.test
            + self.generated
            + self.trivial
            + self.not_public
    }
}

/// The answer, with every way it can be partial named.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// The project has no document index, or it cannot be read.
    pub unavailable: Option<String>,
    pub base: Option<Base>,
    /// Files and hunks of the diff.
    pub files: usize,
    pub hunks: usize,
    /// Changed files in a directory that has no glossary.
    pub files_without_glossary: usize,
    /// Changed files whose nearest glossaries tie: the documents to choose
    /// between.
    pub ambiguous: Vec<(String, Vec<String>)>,
    /// Symbols of the changed files that have a glossary and that the diff
    /// touches.
    pub considered: usize,
    pub skipped: Skipped,
    /// Listed, grouped by the document the entry goes in.
    pub groups: BTreeMap<String, Vec<Missing>>,
    /// Missing symbols beyond the limit.
    pub truncated: usize,
    pub notes: Vec<String>,
}

impl Report {
    fn unavailable(why: impl Into<String>) -> Self {
        Self {
            unavailable: Some(why.into()),
            ..Self::default()
        }
    }

    pub fn listed(&self) -> usize {
        self.groups.values().map(Vec::len).sum()
    }
}

/// Whether a visibility the extractor stored is a public one. `pub(crate)`
/// and `pub(super)` are not.
fn is_public(visibility: &str) -> bool {
    matches!(visibility, "pub" | "public" | "open" | "export")
}

/// A test file by its path: a test directory, or a test naming convention.
fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    let stem = name.split('.').next().unwrap_or(name);
    lower.split('/').any(|dir| {
        matches!(
            dir,
            "test" | "tests" | "__tests__" | "spec" | "specs" | "testdata"
        )
    }) || infigraph_core::lang::is_script_test_name(&lower)
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || camel_test_name(path)
        || name.contains(".test.")
        || name.contains(".spec.")
}

/// `CartTest.java`, `CartTests.cs`, `CartSpec.kt`: a capitalised suffix on the
/// file's stem, which (unlike `latest.rs`) cannot be a word that ends in "test".
fn camel_test_name(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.split('.').next().unwrap_or(name);
    ["Test", "Tests", "Spec", "IT"]
        .iter()
        .any(|suffix| stem.len() > suffix.len() && stem.ends_with(suffix))
}

/// Generated code by its path, or by the marker generators write in the first
/// lines of a file.
fn is_generated(root: &Path, path: &str) -> bool {
    let lower = path.to_lowercase();
    let by_path = lower.split('/').any(|dir| dir == "generated")
        || [
            ".generated.",
            "_generated.",
            ".pb.",
            "_pb2.",
            "_pb.",
            ".g.cs",
            ".designer.cs",
        ]
        .iter()
        .any(|part| lower.contains(part));
    by_path
        || std::fs::read_to_string(root.join(path))
            .map(|text| {
                // Headers are written in every case ("Auto-generated ... do not
                // edit", "DO NOT EDIT", "@generated").
                text.lines().take(5).any(|line| {
                    let line = line.to_lowercase();
                    [
                        "@generated",
                        "do not edit",
                        "code generated",
                        "auto-generated",
                    ]
                    .iter()
                    .any(|marker| line.contains(marker))
                })
            })
            .unwrap_or(false)
}

/// How the hunks of a file cover the lines `start..=end` of a symbol, or
/// `None` when the diff does not touch it.
fn change_of(hunks: &[&Hunk], start: usize, end: usize) -> Option<Change> {
    let mut covered = vec![false; end - start + 1];
    let mut touched = false;
    for hunk in hunks {
        if hunk.new_count == 0 {
            // A pure deletion sits after line `new_start`; inside the symbol
            // it changed it, on its edge it did not.
            touched |= hunk.new_start >= start && hunk.new_start < end;
        } else {
            let (from, to) = (hunk.new_start.max(start), hunk.new_end().min(end));
            if from <= to {
                touched = true;
                covered[from - start..=to - start].fill(true);
            }
        }
    }
    if !touched {
        None
    } else if covered.iter().all(|c| *c) {
        Some(Change::Added)
    } else {
        Some(Change::Changed)
    }
}

/// The symbols the diff since `options.base` adds (or changes) that no
/// glossary entry describes, in directories that have a glossary. Never
/// writes and never starts a daemon.
pub fn missing_docs(root: &Path, options: &Options) -> Report {
    if !docs_switch::docs_enabled(root) || !docs_switch::docs_store_path(root).exists() {
        return Report::unavailable(
            "this project has no document index: run `infigraph index-docs` (a glossary has to \
             be indexed before the directories that keep one are known)",
        );
    }
    let base = match resolve_base(root, options.base.as_deref()) {
        Ok(base) => base,
        Err(why) => return Report::unavailable(why),
    };
    let hunks = match git_blame::diff_hunks(root, &base.commit, GIT_TIMEOUT) {
        Ok(hunks) => hunks,
        Err(BlameError::TimedOut) => {
            return Report::unavailable("git took too long to produce the diff")
        }
        Err(e) => return Report::unavailable(format!("the diff could not be read: {e}")),
    };
    let mut report = Report {
        base: Some(base.clone()),
        ..Report::default()
    };
    let mut by_file: BTreeMap<&str, Vec<&Hunk>> = BTreeMap::new();
    for hunk in hunks.iter().filter(|h| !h.file.is_empty()) {
        if options.scope.contains(&hunk.file) {
            by_file.entry(hunk.file.as_str()).or_default().push(hunk);
        }
    }
    report.files = by_file.len();
    report.hunks = by_file.values().map(Vec::len).sum();
    let head = git_blame::git(root, &["rev-parse", "HEAD"], GIT_TIMEOUT)
        .map(|h| h.trim().to_string())
        .unwrap_or_default();
    if base.commit == head {
        report
            .notes
            .push("the base is HEAD itself: only uncommitted edits are covered".to_string());
    }

    let rows = match link_check::with_docs(root, |q| q.code_link_rows(LinkKind::Documents)) {
        Ok(rows) => rows,
        Err(e) => {
            report.unavailable = Some(format!("the document store could not be read: {e:#}"));
            return report;
        }
    };

    let mut doc_of: HashMap<&str, String> = HashMap::new();
    for file in by_file.keys() {
        match glossary_for(&rows, file) {
            Glossary::One(doc) => {
                doc_of.insert(file, doc);
            }
            Glossary::Tie(docs) => report.ambiguous.push((file.to_string(), docs)),
            Glossary::None => report.files_without_glossary += 1,
        }
    }
    let files: Vec<&str> = doc_of.keys().copied().collect();
    let symbols = match link_check::symbols_in_files(root, &files) {
        Ok(symbols) => symbols,
        Err(e) => {
            report.unavailable = Some(format!("{e:#}"));
            return report;
        }
    };

    let mut found: Vec<Missing> = Vec::new();
    let mut doc_text: HashMap<String, Vec<String>> = HashMap::new();
    for (file, doc_file) in &doc_of {
        let Some(symbols) = symbols.get(*file) else {
            continue;
        };
        let touching = &by_file[file];
        let generated = std::cell::OnceCell::new();
        for symbol in symbols {
            let Some(change) = change_of(touching, symbol.start, symbol.end) else {
                continue;
            };
            report.considered += 1;
            if keeps(
                root,
                options,
                &rows,
                &mut doc_text,
                (file, doc_file),
                symbol,
                change,
                &generated,
                &mut report.skipped,
            ) {
                found.push(Missing {
                    symbol: symbol.id.clone(),
                    kind: symbol.kind.to_lowercase(),
                    file: file.to_string(),
                    line: symbol.start,
                    change,
                    doc_file: doc_file.clone(),
                });
            }
        }
    }
    debug_assert_eq!(
        report.considered,
        found.len() + report.skipped.total(),
        "every considered symbol is listed or counted once"
    );
    found.sort_by(|a, b| {
        (&a.doc_file, &a.file, a.line, &a.symbol).cmp(&(&b.doc_file, &b.file, b.line, &b.symbol))
    });
    report.truncated = found.len().saturating_sub(options.limit);
    found.truncate(options.limit);
    for missing in found {
        report
            .groups
            .entry(missing.doc_file.clone())
            .or_default()
            .push(missing);
    }
    report.ambiguous.sort();
    report
}

/// Whether `symbol` belongs in the list; if not, counts the first reason.
#[allow(clippy::too_many_arguments)]
fn keeps(
    root: &Path,
    options: &Options,
    rows: &[CodeLinkRow],
    doc_text: &mut HashMap<String, Vec<String>>,
    (file, doc_file): (&str, &str),
    symbol: &FileSymbol,
    change: Change,
    generated: &std::cell::OnceCell<bool>,
    skipped: &mut Skipped,
) -> bool {
    if !DECLARATIONS.contains(&symbol.kind.to_lowercase().as_str()) {
        skipped.not_a_declaration += 1;
        return false;
    }
    if change == Change::Changed && !options.include_changed {
        skipped.changed += 1;
        return false;
    }
    // The index may be behind the file: the candidate document's own text is
    // the answer for the entries it holds.
    let keys = doc_text.entry(doc_file.to_string()).or_insert_with(|| {
        std::fs::read_to_string(root.join(doc_file))
            .map(|text| entries(&text).into_iter().map(|e| e.key).collect())
            .unwrap_or_default()
    });
    if keys.contains(&symbol.id) || rows.iter().any(|r| r.target == symbol.id) {
        skipped.documented += 1;
        return false;
    }
    if is_test_path(file) {
        skipped.test += 1;
        return false;
    }
    if *generated.get_or_init(|| is_generated(root, file)) {
        skipped.generated += 1;
        return false;
    }
    if symbol.end - symbol.start + 1 < options.min_lines {
        skipped.trivial += 1;
        return false;
    }
    if !options.all_visibility {
        match symbol.visibility.as_deref() {
            Some(v) if is_public(v) => {}
            Some(_) => {
                skipped.not_public += 1;
                return false;
            }
            None => {
                skipped.not_public += 1;
                skipped.unknown_visibility += 1;
                return false;
            }
        }
    }
    true
}

/// The `doc_entry` call that would create `missing`'s entry: a dry run until
/// `apply` is added. `text` is left for the reader, since infigraph does not
/// write the prose.
pub fn hint(missing: &Missing) -> String {
    format!(
        "doc_entry {}",
        serde_json::json!({
            "op": "create",
            "target": missing.symbol,
            "doc_file": missing.doc_file,
            "text": "<what it is for and what a caller must know>",
        })
    )
}

impl Report {
    pub fn render(&self) -> String {
        if let Some(why) = &self.unavailable {
            return format!("missing_docs: unavailable: {why}\n");
        }
        let mut out = String::new();
        if let Some(base) = &self.base {
            out.push_str(&format!(
                "base: {} ({}), diffed from its merge-base {} to the working tree: {} files, {} hunks\n",
                base.name,
                base.via,
                &base.commit[..base.commit.len().min(8)],
                self.files,
                self.hunks
            ));
        }
        for note in &self.notes {
            out.push_str(&format!("note: {note}\n"));
        }
        let listed = self.listed();
        if listed == 0 {
            out.push_str("no symbol of this diff is missing a glossary entry\n");
        } else {
            out.push_str(&format!(
                "{listed} symbol(s) with no glossary entry, by the document the entry goes in:\n"
            ));
        }
        for (doc, items) in &self.groups {
            out.push_str(&format!("\n{doc} ({} missing)\n", items.len()));
            for m in items {
                out.push_str(&format!(
                    "  {}:{}  {} {}{}\n    {}\n",
                    m.file,
                    m.line,
                    m.kind,
                    m.symbol,
                    if m.change == Change::Changed {
                        "  [changed, not added]"
                    } else {
                        ""
                    },
                    hint(m)
                ));
            }
        }
        if self.truncated > 0 {
            out.push_str(&format!(
                "\n... and {} more (raise `limit` or narrow `path_prefix`)\n",
                self.truncated
            ));
        }
        let s = &self.skipped;
        out.push_str(&format!(
            "\nconsidered {} symbols the diff touches in directories with a glossary; left out: \
             {} already documented, {} not declarations, {} changed rather than added (include_changed \
             lists them), {} in tests, {} generated, {} under the minimum length, {} not public \
             ({} of them with no visibility recorded; visibility=all lists them)\n",
            self.considered,
            s.documented,
            s.not_a_declaration,
            s.changed,
            s.test,
            s.generated,
            s.trivial,
            s.not_public,
            s.unknown_visibility,
        ));
        if self.files_without_glossary > 0 {
            out.push_str(&format!(
                "{} changed file(s) are in directories with no glossary and were not looked at\n",
                self.files_without_glossary
            ));
        }
        for (file, docs) in &self.ambiguous {
            out.push_str(&format!(
                "{file}: its nearest glossaries tie ({}); pass doc_file to `doc_entry create`\n",
                docs.join(", ")
            ));
        }
        out.push_str(
            "a moved function reads as added (a move is not detected); an entry for a type does \
             not cover its new methods\n",
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(new_start: usize, new_count: usize) -> Hunk {
        let mut diff =
            format!("diff --git a/f b/f\n+++ b/f\n@@ -1,0 +{new_start},{new_count} @@\n");
        for _ in 0..new_count {
            diff.push_str("+x\n");
        }
        git_blame::parse_hunks(&diff).remove(0)
    }

    #[test]
    fn a_symbol_wholly_inside_new_lines_is_added() {
        let h = hunk(10, 8);
        assert_eq!(change_of(&[&h], 10, 17), Some(Change::Added));
        assert_eq!(change_of(&[&h], 12, 15), Some(Change::Added));
    }

    #[test]
    fn a_symbol_only_partly_inside_new_lines_is_changed() {
        let h = hunk(12, 2);
        assert_eq!(change_of(&[&h], 10, 20), Some(Change::Changed));
        assert_eq!(change_of(&[&h], 13, 20), Some(Change::Changed));
    }

    #[test]
    fn a_symbol_the_diff_does_not_touch_is_nothing() {
        let h = hunk(30, 2);
        assert_eq!(change_of(&[&h], 10, 20), None);
        assert_eq!(change_of(&[&h], 31, 40), Some(Change::Changed));
    }

    #[test]
    fn two_hunks_that_together_cover_a_symbol_make_it_added() {
        let (a, b) = (hunk(10, 3), hunk(13, 3));
        assert_eq!(change_of(&[&a, &b], 10, 15), Some(Change::Added));
    }

    #[test]
    fn only_a_plain_pub_or_public_is_public() {
        for v in ["pub", "public", "open", "export"] {
            assert!(is_public(v), "{v}");
        }
        for v in [
            "pub(crate)",
            "pub(super)",
            "private",
            "protected",
            "internal",
        ] {
            assert!(!is_public(v), "{v}");
        }
    }

    #[test]
    fn test_files_are_known_by_directory_and_by_name() {
        for p in [
            "tests/a.rs",
            "src/__tests__/a.ts",
            "pkg/test_cart.py",
            "pkg/cart_test.go",
            "web/cart.spec.ts",
            "web/cart.test.js",
            "src/CartTests.cs",
        ] {
            assert!(is_test_path(p), "{p}");
        }
        for p in [
            "src/cart.rs",
            "src/latest.rs",
            "lib/util.py",
            "src/contest/a.rs",
        ] {
            assert!(!is_test_path(p), "{p}");
        }
    }
}
