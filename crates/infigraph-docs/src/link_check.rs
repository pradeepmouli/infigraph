//! `doctor`'s checks of the links between documents and code.
//!
//! - **Keys**: every documents key must resolve to a live symbol (or, for
//!   `<file>::module`, to a file in the graph). A key that does not is
//!   rename rot, and the report says which kind: a spelling the indexer does
//!   not use (`A.b` for `A::b`), a symbol the file has several of (`name#N`),
//!   a file the graph does not have, or a name that is gone. Always runs.
//! - **Sync** (`--docs-sync`): which entries describe code that changed after
//!   they did, from `git blame` (`infigraph_core::git_blame`, the one
//!   definition `get_doc_context` uses too). It costs a blame per file, so it
//!   runs only when asked, within a time budget; past the budget the rest is
//!   judged per file, which is cheap and under-reports, and the report says so
//!   and counts it apart from the exact answers.
//!
//! The check lives here, not in core's `doctor`, because core cannot read the
//! document store. It reads both stores as an observer: through a live daemon
//! without leasing it, otherwise directly, and it never starts a daemon.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use infigraph_core::docs_switch;
use infigraph_core::doctor::CheckResult;
use infigraph_core::escape_str;
use infigraph_core::git_blame::SyncState;
use infigraph_core::graph::observe::ObservedGraph;
use infigraph_core::graph::query_exec::{LocalExec, QueryExec};
use infigraph_core::graph::remote_exec::RemoteExec;

use crate::code_links::{CodeLinkRow, LinkKind, CODE_LINKS_VERSION};
use crate::query::DocQuery;
use crate::stale_docs::{judge, rank, Judgement, COARSE_UNDER_REPORTS};
use crate::store::DocStore;

pub const CATEGORY: &str = "document links";

/// `--docs-sync` judges exactly until this much time has passed.
pub const SYNC_BUDGET: Duration = Duration::from_secs(120);

const BATCH: usize = 400;
const EXAMPLES: usize = 10;
const HINT_CAP: usize = 200;

/// The document store as an observer reads it.
pub(crate) fn with_docs<T>(
    root: &Path,
    f: impl FnOnce(&DocQuery<&dyn QueryExec>) -> Result<T>,
) -> Result<T> {
    let watch_lock = root.join(".infigraph").join("watch.lock");
    if infigraph_core::daemon::lifecycle::daemon_is_alive(&watch_lock) {
        let remote = RemoteExec::docs_observer(root);
        let exec: &dyn QueryExec = &remote;
        f(&DocQuery::new_with(exec))
    } else {
        let store = DocStore::open_for_read(root)?;
        let conn = store.connection()?;
        let local = LocalExec::new(&conn);
        let exec: &dyn QueryExec = &local;
        f(&DocQuery::new_with(exec))
    }
}

/// A live symbol a key resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Live {
    pub file: String,
    pub start: usize,
    pub end: usize,
}

/// Why a key resolves to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Miss {
    /// The key spells a qualifier with `.`; the graph's id uses `::`.
    Dotted { suggestion: String },
    /// The file has several symbols of this name, told apart only by `#N`.
    Numbered { count: usize },
    /// The key's file is not in the graph.
    FileNotIndexed,
    /// The file is indexed and has no such symbol.
    NoSuchSymbol,
}

impl Miss {
    fn reason(&self) -> &'static str {
        match self {
            Miss::Dotted { .. } => "written with `.` where the graph uses `::`",
            Miss::Numbered { .. } => "several symbols of that name in the file (`name#N` ids)",
            Miss::FileNotIndexed => "file is not in the graph",
            Miss::NoSuchSymbol => "no such symbol in the file (renamed or removed)",
        }
    }

    fn hint(&self, key: &str) -> String {
        match self {
            Miss::Dotted { suggestion } => format!("{key}  ->  did you mean {suggestion}"),
            Miss::Numbered { count } => format!(
                "{key}  ->  {count} symbols have that name; `#N` ids depend on source order and \
                 cannot be keys (this is how members of an object literal nothing names, and \
                 overloads, are numbered: do not key an entry on one)"
            ),
            Miss::NoSuchSymbol if ends_in_ordinal(key) => format!(
                "{key}  ->  a `#N` id follows source order and moves when the code does: do not \
                 key an entry on one"
            ),
            _ => key.to_string(),
        }
    }
}

/// A key that ends in `#<digits>`.
fn ends_in_ordinal(key: &str) -> bool {
    key.rsplit_once('#')
        .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn rows_of(exec: &dyn QueryExec, cypher: &str) -> Vec<Vec<String>> {
    exec.query_rows(cypher).unwrap_or_default()
}

/// Which of `keys` are live symbols (id -> where), and which `<file>::module`
/// keys name a file the graph has.
fn resolve(exec: &dyn QueryExec, keys: &[&str]) -> (HashMap<String, Live>, Vec<String>) {
    let mut live = HashMap::new();
    let mut modules = Vec::new();
    for batch in keys.chunks(BATCH) {
        let list = batch
            .iter()
            .map(|k| format!("'{}'", escape_str(k)))
            .collect::<Vec<_>>()
            .join(", ");
        for row in rows_of(
            exec,
            &format!(
                "MATCH (s:Symbol) WHERE s.id IN [{list}] \
                 RETURN s.id, s.file, s.start_line, s.end_line"
            ),
        ) {
            if let [id, file, start, end] = &row[..] {
                if let (Ok(start), Ok(end)) = (start.parse(), end.parse()) {
                    live.insert(
                        id.clone(),
                        Live {
                            file: file.clone(),
                            start,
                            end,
                        },
                    );
                }
            }
        }
        let files: Vec<String> = batch
            .iter()
            .filter(|k| !live.contains_key(**k))
            .filter_map(|k| k.strip_suffix("::module"))
            .map(|f| format!("'{}'", escape_str(f)))
            .collect();
        if !files.is_empty() {
            for row in rows_of(
                exec,
                &format!(
                    "MATCH (f:File) WHERE f.path IN [{}] RETURN f.path",
                    files.join(", ")
                ),
            ) {
                modules.extend(row.into_iter().next().map(|f| format!("{f}::module")));
            }
        }
    }
    (live, modules)
}

fn classify(exec: &dyn QueryExec, key: &str) -> Miss {
    let (file, name) = key.split_once("::").unwrap_or((key, ""));
    let exists = |id: &str| {
        !rows_of(
            exec,
            &format!(
                "MATCH (s:Symbol) WHERE s.id = '{}' RETURN s.id LIMIT 1",
                escape_str(id)
            ),
        )
        .is_empty()
    };
    if name.contains('.') {
        let suggestion = format!("{file}::{}", name.replace('.', "::"));
        if exists(&suggestion) {
            return Miss::Dotted { suggestion };
        }
    }
    // `name0` is the counter an indexer glues onto an object-literal key; the
    // symbols are `name#N` now, so a key that is a numbered name's base with
    // digits on it is the same mistake.
    let base = key.trim_end_matches(|c: char| c.is_ascii_digit());
    let base = if base.len() < key.len() && base.contains("::") && !base.ends_with("::") {
        base
    } else {
        key
    };
    let numbered = rows_of(
        exec,
        &format!(
            "MATCH (s:Symbol) WHERE s.id STARTS WITH '{}#' RETURN count(s)",
            escape_str(base)
        ),
    );
    let count: usize = numbered
        .first()
        .and_then(|r| r.first())
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    if count > 0 {
        return Miss::Numbered { count };
    }
    let file_known = !rows_of(
        exec,
        &format!(
            "MATCH (f:File) WHERE f.path = '{}' RETURN f.path LIMIT 1",
            escape_str(file)
        ),
    )
    .is_empty();
    if file_known {
        Miss::NoSuchSymbol
    } else {
        Miss::FileNotIndexed
    }
}

/// What the scan found besides keys that do not resolve.
#[derive(Default, Clone, Copy)]
struct Scanned<'a> {
    /// Headings that start like a key and are not exactly one.
    near_misses: &'a [CodeLinkRow],
    /// Code fences opened and never closed.
    unclosed: &'a [CodeLinkRow],
    /// Key headings inside a code fence (`line_end` is the fence's opener).
    fenced: &'a [CodeLinkRow],
    /// The `documents` rows, to find a key that has more than one entry.
    documents: &'a [CodeLinkRow],
}

/// The keys with more than one entry, each with `file:line` of every one.
fn duplicated(documents: &[CodeLinkRow]) -> Vec<(&str, Vec<String>)> {
    let mut by_key: BTreeMap<&str, Vec<&CodeLinkRow>> = BTreeMap::new();
    for row in documents {
        by_key.entry(row.target.as_str()).or_default().push(row);
    }
    by_key
        .into_iter()
        .filter(|(_, rows)| rows.len() > 1)
        .map(|(key, mut rows)| {
            rows.sort_by(|a, b| (&a.doc_file, a.line_start).cmp(&(&b.doc_file, b.line_start)));
            (
                key,
                rows.iter()
                    .map(|r| format!("{}:{}", r.doc_file, r.line_start))
                    .collect(),
            )
        })
        .collect()
}

/// The key-resolution result: how many keys resolve, and the rest by reason
/// with examples and hints.
fn keys_result(
    project: &Path,
    total: usize,
    unresolved: &[(String, Miss)],
    scanned: &Scanned,
) -> CheckResult {
    let Scanned {
        near_misses,
        unclosed,
        fenced,
        documents,
    } = *scanned;
    let name = format!("{}: document keys", project.display());
    let resolved = total - unresolved.len();
    let mut notes = Vec::new();
    let dups = duplicated(documents);
    if !dups.is_empty() {
        let mut note = format!(
            "{} key(s) have more than one entry; doc_entry refuses to pick one (pass nth, line or \
             doc_file), and the staleness of each is judged against the same symbol",
            dups.len()
        );
        for (key, at) in dups.iter().take(EXAMPLES) {
            note.push_str(&format!("\n    {key}  ->  {}", at.join(", ")));
        }
        if dups.len() > EXAMPLES {
            note.push_str(&format!("\n    ... and {} more", dups.len() - EXAMPLES));
        }
        notes.push(note);
    }
    if !unclosed.is_empty() {
        // One line per file: its first fence that nothing closes.
        let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
        for r in unclosed {
            let first = per_file.entry(r.doc_file.as_str()).or_insert(r.line_start);
            *first = (*first).min(r.line_start);
        }
        let mut note = format!(
            "{} document(s) open a code fence that is never closed, so its lines are read as \
             prose, not as code",
            per_file.len()
        );
        for (file, line) in per_file.iter().take(EXAMPLES) {
            note.push_str(&format!("\n    unclosed code fence at {file}:{line}"));
        }
        if per_file.len() > EXAMPLES {
            note.push_str(&format!("\n    ... and {} more", per_file.len() - EXAMPLES));
        }
        notes.push(note);
    }
    if !fenced.is_empty() {
        let mut rows: Vec<&CodeLinkRow> = fenced.iter().collect();
        rows.sort_by(|a, b| (&a.doc_file, a.line_start).cmp(&(&b.doc_file, b.line_start)));
        let mut note = format!(
            "{} key heading(s) sit inside a code fence, so they are not entries",
            rows.len()
        );
        for r in rows.iter().take(EXAMPLES) {
            note.push_str(&format!(
                "\n    {}:{} has the key heading `{}`, but it is inside a code fence opened at \
                 line {} and closed at line {}; close or remove the opener, or make it a \
                 ```markdown fence if the heading is an example",
                r.doc_file, r.line_start, r.target, r.line_end, r.text
            ));
        }
        if rows.len() > EXAMPLES {
            note.push_str(&format!("\n    ... and {} more", rows.len() - EXAMPLES));
        }
        notes.push(note);
    }
    if !near_misses.is_empty() {
        let examples: Vec<String> = near_misses
            .iter()
            .take(3)
            .map(|r| format!("{}:{}  {}", r.doc_file, r.line_start, r.target))
            .collect();
        notes.push(format!(
            "{} heading(s) start like a key but are not exactly one `file::name` span, so they \
             link nothing (e.g. {})",
            near_misses.len(),
            examples.join("; ")
        ));
    }
    if unresolved.is_empty() && notes.is_empty() {
        return CheckResult::pass(
            CATEGORY,
            name,
            format!("{total} of {total} documents keys resolve to a live symbol"),
        );
    }
    let mut message = format!("{resolved} of {total} documents keys resolve to a live symbol");
    let mut by_reason: BTreeMap<&str, Vec<&(String, Miss)>> = BTreeMap::new();
    for miss in unresolved {
        by_reason.entry(miss.1.reason()).or_default().push(miss);
    }
    for (reason, misses) in &by_reason {
        message.push_str(&format!("\n  {} key(s): {reason}", misses.len()));
        for (key, miss) in misses.iter().take(EXAMPLES).map(|m| (&m.0, &m.1)) {
            message.push_str(&format!("\n    {}", miss.hint(key)));
        }
        if misses.len() > EXAMPLES {
            message.push_str(&format!("\n    ... and {} more", misses.len() - EXAMPLES));
        }
    }
    for note in notes {
        message.push_str(&format!("\n  {note}"));
    }
    CheckResult::warn(
        CATEGORY,
        name,
        message,
        "fix or remove the keys: a key is a symbol id, `file::qualified::name`, as the graph \
         spells it",
    )
}

fn sync_result(
    root: &Path,
    entries: &[(CodeLinkRow, Live)],
    started: Instant,
    budget: Duration,
) -> CheckResult {
    let name = format!("{}: doc/code sync", root.display());
    let Judgement {
        judged,
        unknown,
        ack_error,
        ..
    } = judge(root, entries, started, budget);

    let stale = |exact: bool| {
        judged
            .iter()
            .filter(|j| j.exact == exact && j.state != SyncState::InSync)
            .count()
    };
    let (n_exact, n_coarse) = (
        judged.iter().filter(|j| j.exact).count(),
        judged.iter().filter(|j| !j.exact).count(),
    );
    let mut message = format!(
        "{} of {} entries possibly stale (exact git blame of {} entries)",
        stale(true),
        n_exact,
        n_exact
    );
    let acknowledged = judged.iter().filter(|j| j.acknowledged).count();
    if acknowledged > 0 {
        message.push_str(&format!("; {acknowledged} acknowledged"));
    }
    if let Some(why) = ack_error {
        message.push_str(&format!(
            "\n  acknowledgements could not be read ({why}): entries a reviewer cleared may be \
             listed"
        ));
    }
    if n_coarse > 0 {
        message.push_str(&format!(
            "\n  plus {} of {} entries judged per file because the {}s budget ran out or git \
             failed on their files (coarse; {COARSE_UNDER_REPORTS})",
            stale(false),
            n_coarse,
            budget.as_secs()
        ));
    }
    if unknown > 0 {
        message.push_str(&format!(
            "\n  {unknown} entries could not be judged: git gave no answer for their files"
        ));
    }
    let stale_examples = rank(&judged);
    for j in stale_examples.iter().take(EXAMPLES) {
        let label = j.state.label().unwrap_or_default();
        message.push_str(&format!(
            "\n    {} ({}:{}){} {label}",
            j.key,
            j.doc_file,
            j.doc_lines.0,
            if j.exact { "" } else { " [coarse]" },
        ));
    }
    if stale_examples.len() > EXAMPLES {
        message.push_str(&format!(
            "\n    ... and {} more",
            stale_examples.len() - EXAMPLES
        ));
    }
    if stale(true) + stale(false) == 0 && n_coarse == 0 && unknown == 0 {
        CheckResult::pass(CATEGORY, name, message)
    } else {
        CheckResult::warn(
            CATEGORY,
            name,
            message,
            "'possibly stale' is a prompt to re-read the entry against the code, not an error: \
             update the entry, or touch it once it is confirmed current",
        )
    }
}

/// The live symbols among `keys` (key -> where), read as an observer reads the
/// graph.
pub(crate) fn live_symbols(root: &Path, keys: &[&str]) -> Result<HashMap<String, Live>> {
    let graph_path = root.join(".infigraph").join("graph");
    let observed = ObservedGraph::open(root, &graph_path)?;
    observed.with_exec(|exec| resolve(exec, keys).0)
}

/// Whether `key` resolves the way `doctor` judges it: a symbol the graph has,
/// or a `<file>::module` key whose file the graph has.
pub(crate) fn key_is_live(root: &Path, key: &str) -> Result<bool> {
    let graph_path = root.join(".infigraph").join("graph");
    let observed = ObservedGraph::open(root, &graph_path)?;
    observed.with_exec(|exec| {
        let (live, modules) = resolve(exec, &[key]);
        live.contains_key(key) || modules.iter().any(|m| m == key)
    })
}

/// Every check of one project's document links; empty when the project has no
/// documents or no links to check. `docs_sync` adds the git-based sync check.
pub fn check_project(root: &Path, docs_sync: bool) -> Vec<CheckResult> {
    if !docs_switch::docs_enabled(root) || !docs_switch::docs_store_path(root).exists() {
        return Vec::new();
    }
    let started = Instant::now();
    let name = |what: &str| format!("{}: {what}", root.display());

    let read = with_docs(root, |q| {
        Ok((
            q.code_link_rows(LinkKind::Documents)?,
            q.code_link_rows(LinkKind::NearMiss)?,
            q.code_link_rows(LinkKind::UnclosedFence)?,
            q.code_link_rows(LinkKind::FencedKey)?,
            q.get_doc_hashes()?,
            q.code_link_scans()?,
        ))
    });
    let (documents, near_misses, unclosed, fenced, stored, scans) = match read {
        Ok(read) => read,
        Err(e) => {
            return vec![CheckResult::warn(
                CATEGORY,
                name("document links"),
                format!("the document store could not be read: {e:#}"),
                "run `infigraph index-docs`",
            )]
        }
    };

    let mut results = Vec::new();
    let local: Vec<&String> = stored
        .keys()
        .filter(|id| !id.is_empty() && !id.contains("://"))
        .collect();
    let unscanned = local
        .iter()
        .filter(|id| scans.get(**id).copied().unwrap_or(0) < CODE_LINKS_VERSION)
        .count();
    if unscanned > 0 {
        results.push(CheckResult::warn(
            CATEGORY,
            name("scan"),
            format!(
                "{unscanned} of {} documents have not been scanned for code links",
                local.len()
            ),
            "run `infigraph index-docs`",
        ));
    }
    if documents.is_empty() && near_misses.is_empty() && unclosed.is_empty() && fenced.is_empty() {
        return results;
    }

    let graph_path = root.join(".infigraph").join("graph");
    let observed = match ObservedGraph::open(root, &graph_path) {
        Ok(observed) => observed,
        Err(e) => {
            results.push(CheckResult::warn(
                CATEGORY,
                name("document keys"),
                format!(
                    "{} documents keys not checked: the graph could not be read: {e:#}",
                    documents.len()
                ),
                "run `infigraph doctor` for the graph's state",
            ));
            return results;
        }
    };
    let keys: Vec<&str> = documents.iter().map(|r| r.target.as_str()).collect();
    let outcome = observed.with_exec(|exec| {
        let (live, modules) = resolve(exec, &keys);
        let mut unresolved: Vec<(String, Miss)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for key in &keys {
            if live.contains_key(*key) || modules.iter().any(|m| m == key) || !seen.insert(*key) {
                continue;
            }
            let miss = if unresolved.len() < HINT_CAP {
                classify(exec, key)
            } else {
                Miss::NoSuchSymbol
            };
            unresolved.push((key.to_string(), miss));
        }
        (live, unresolved)
    });
    let (live, unresolved) = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            results.push(CheckResult::warn(
                CATEGORY,
                name("document keys"),
                format!("documents keys not checked: {e:#}"),
                "run `infigraph doctor` for the graph's state",
            ));
            return results;
        }
    };
    let distinct: std::collections::HashSet<&str> = keys.iter().copied().collect();
    results.push(keys_result(
        root,
        distinct.len(),
        &unresolved,
        &Scanned {
            near_misses: &near_misses,
            unclosed: &unclosed,
            fenced: &fenced,
            documents: &documents,
        },
    ));

    if docs_sync {
        let entries: Vec<(CodeLinkRow, Live)> = documents
            .iter()
            .filter_map(|r| Some((r.clone(), live.get(&r.target)?.clone())))
            .collect();
        results.push(sync_result(root, &entries, started, SYNC_BUDGET));
    } else if !documents.is_empty() {
        results.push(CheckResult::pass(
            CATEGORY,
            name("doc/code sync"),
            "not checked (run `infigraph doctor --docs-sync`)",
        ));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dotted_key_gets_the_double_colon_spelling_as_its_hint() {
        let miss = Miss::Dotted {
            suggestion: "a.ts::A::b".to_string(),
        };
        assert_eq!(
            miss.hint("a.ts::A.b"),
            "a.ts::A.b  ->  did you mean a.ts::A::b"
        );
    }

    #[test]
    fn a_numbered_key_says_how_many_symbols_share_the_name() {
        let hint = Miss::Numbered { count: 5 }.hint("types.ts::rules");
        assert!(hint.contains("5 symbols") && hint.contains("#N"), "{hint}");
    }

    #[test]
    fn a_numbered_hint_says_not_to_key_an_entry_on_a_number() {
        let hint = Miss::Numbered { count: 5 }.hint("a.ts::name");
        assert!(
            hint.contains("object literal") && hint.contains("do not key"),
            "{hint}"
        );
        let gone = Miss::NoSuchSymbol.hint("a.ts::name#4");
        assert!(
            gone.contains("source order") && gone.contains("do not"),
            "{gone}"
        );
        assert_eq!(Miss::NoSuchSymbol.hint("a.ts::name"), "a.ts::name");
        assert_eq!(Miss::NoSuchSymbol.hint("a.ts::name#x"), "a.ts::name#x");
    }

    #[test]
    fn all_keys_resolving_is_a_pass_and_anything_else_a_warn_with_examples() {
        let project = Path::new("/p");
        let pass = keys_result(project, 3, &[], &Scanned::default());
        assert_eq!(pass.status, infigraph_core::doctor::CheckStatus::Pass);
        assert!(pass.message.contains("3 of 3"));

        let miss = vec![
            (
                "a.ts::A.b".to_string(),
                Miss::Dotted {
                    suggestion: "a.ts::A::b".into(),
                },
            ),
            ("a.ts::gone".to_string(), Miss::NoSuchSymbol),
        ];
        let warn = keys_result(project, 5, &miss, &Scanned::default());
        assert_eq!(warn.status, infigraph_core::doctor::CheckStatus::Warn);
        assert!(warn.message.contains("3 of 5"), "{}", warn.message);
        assert!(warn.message.contains("did you mean a.ts::A::b"));
        assert!(warn.message.contains("a.ts::gone"));
    }

    fn key_row(key: &str, file: &str, line: usize) -> CodeLinkRow {
        CodeLinkRow {
            kind: LinkKind::Documents,
            target: key.to_string(),
            ..unclosed(file, line)
        }
    }

    #[test]
    fn a_key_with_two_entries_is_listed_with_the_file_and_line_of_each() {
        let rows = vec![
            key_row("a.ts::f", "docs/b.md", 30),
            key_row("a.ts::g", "docs/a.md", 5),
            key_row("a.ts::f", "docs/a.md", 12),
        ];
        let got = keys_result(
            Path::new("/p"),
            2,
            &[],
            &Scanned {
                documents: &rows,
                ..Default::default()
            },
        );
        assert_eq!(got.status, infigraph_core::doctor::CheckStatus::Warn);
        assert!(
            got.message.contains("1 key(s) have more than one entry"),
            "{}",
            got.message
        );
        assert!(
            got.message
                .contains("a.ts::f  ->  docs/a.md:12, docs/b.md:30"),
            "{}",
            got.message
        );
        assert!(!got.message.contains("a.ts::g  ->"), "{}", got.message);
    }

    fn unclosed(file: &str, line: usize) -> CodeLinkRow {
        CodeLinkRow {
            id: format!("{file}#{line}"),
            doc_file: file.to_string(),
            kind: LinkKind::UnclosedFence,
            target: "```".to_string(),
            heading: String::new(),
            line_start: line,
            line_end: line,
            text: String::new(),
        }
    }

    #[test]
    fn a_key_heading_inside_a_fence_is_listed_with_its_opener() {
        let row = |line: usize, opener: usize| CodeLinkRow {
            id: format!("docs/a.md#{line}"),
            doc_file: "docs/a.md".to_string(),
            kind: LinkKind::FencedKey,
            target: "a.ts::one".to_string(),
            heading: "`a.ts::one`".to_string(),
            line_start: line,
            line_end: opener,
            text: "13".to_string(),
        };
        let rows = vec![row(6, 3), row(10, 3)];
        let got = keys_result(
            Path::new("/p"),
            0,
            &[],
            &Scanned {
                fenced: &rows,
                ..Default::default()
            },
        );
        assert_eq!(got.status, infigraph_core::doctor::CheckStatus::Warn);
        assert!(
            got.message.contains(
                "docs/a.md:6 has the key heading `a.ts::one`, but it is inside a code fence \
                 opened at line 3 and closed at line 13"
            ),
            "{}",
            got.message
        );
        assert!(got.message.contains("docs/a.md:10"), "{}", got.message);
    }

    #[test]
    fn an_unclosed_fence_is_one_line_per_file_at_its_first_opening() {
        let rows = vec![
            unclosed("docs/a.md", 40),
            unclosed("docs/a.md", 12),
            unclosed("docs/b.md", 7),
        ];
        let got = keys_result(
            Path::new("/p"),
            0,
            &[],
            &Scanned {
                unclosed: &rows,
                ..Default::default()
            },
        );
        assert_eq!(got.status, infigraph_core::doctor::CheckStatus::Warn);
        assert!(got.message.contains("2 document(s)"), "{}", got.message);
        assert!(
            got.message.contains("unclosed code fence at docs/a.md:12"),
            "{}",
            got.message
        );
        assert!(!got.message.contains("docs/a.md:40"), "{}", got.message);
        assert!(
            got.message.contains("unclosed code fence at docs/b.md:7"),
            "{}",
            got.message
        );
    }

    /// A repository where `code.py` changed after `doc.md` did, and one entry
    /// that links them.
    fn stale_repo() -> (tempfile::TempDir, Vec<(CodeLinkRow, Live)>) {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str], date: &str| {
            let out = std::process::Command::new("git")
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        };
        git(&["init", "-q"], "2024-01-01T00:00:00Z");
        std::fs::write(dir.path().join("doc.md"), "### `code.py::f`\n\nWhy f.\n").unwrap();
        std::fs::write(dir.path().join("code.py"), "def f():\n    return 1\n").unwrap();
        git(&["add", "."], "2024-01-01T00:00:00Z");
        git(&["commit", "-q", "-m", "one"], "2024-01-01T00:00:00Z");
        std::fs::write(dir.path().join("code.py"), "def f():\n    return 2\n").unwrap();
        git(&["commit", "-q", "-am", "two"], "2024-06-01T00:00:00Z");
        let row = CodeLinkRow {
            id: "doc.md#0".into(),
            doc_file: "doc.md".into(),
            kind: LinkKind::Documents,
            target: "code.py::f".into(),
            heading: "`code.py::f`".into(),
            line_start: 1,
            line_end: 3,
            text: String::new(),
        };
        let live = Live {
            file: "code.py".into(),
            start: 1,
            end: 2,
        };
        (dir, vec![(row, live)])
    }

    #[test]
    fn within_its_budget_the_sync_check_is_exact() {
        let (dir, entries) = stale_repo();
        let result = sync_result(dir.path(), &entries, Instant::now(), SYNC_BUDGET);
        assert!(
            result
                .message
                .contains("1 of 1 entries possibly stale (exact git blame"),
            "{}",
            result.message
        );
        assert!(!result.message.contains("coarse"), "{}", result.message);
        assert!(
            result.message.contains("code changed 2024-06-01"),
            "{}",
            result.message
        );
    }

    #[test]
    fn past_its_budget_the_rest_is_judged_per_file_and_said_to_under_report() {
        let (dir, entries) = stale_repo();
        let result = sync_result(dir.path(), &entries, Instant::now(), Duration::ZERO);
        let message = &result.message;
        assert!(
            message.contains("0 of 0 entries possibly stale"),
            "{message}"
        );
        assert!(
            message.contains("plus 1 of 1 entries judged per file")
                && message.contains("coarse")
                && message.contains("under-report"),
            "coarse results must be labelled and counted apart:\n{message}"
        );
        assert!(message.contains("[coarse]"), "{message}");
    }
}
