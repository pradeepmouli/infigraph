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

/// A live symbol a key resolved to: its file and the first and last line of
/// its declaration. A key that names a group of same-named symbols (see
/// [`group_query`]) resolves to every member's span, so staleness is judged
/// over all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Live {
    pub file: String,
    pub spans: Vec<(usize, usize)>,
}

impl Live {
    pub(crate) fn single(file: &str, start: usize, end: usize) -> Self {
        Live {
            file: file.to_string(),
            spans: vec![(start, end)],
        }
    }

    /// The first line of the first span and the last line of the last one: the
    /// lines of a symbol, or of a group from its first member to its last.
    pub(crate) fn hull(&self) -> (usize, usize) {
        let start = self.spans.iter().map(|s| s.0).min().unwrap_or(0);
        let end = self.spans.iter().map(|s| s.1).max().unwrap_or(0);
        (start, end)
    }

    /// Every span as `start-end`, for a message: `12-20, 40-44`.
    pub(crate) fn lines_text(&self) -> String {
        self.spans
            .iter()
            .map(|(s, e)| format!("{s}-{e}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The query that reads the members of the groups `keys` name: every symbol
/// whose id is `key#<something>`. Shared by [`resolve`] and by `linked`, which
/// reads the same members through a backend rather than an executor.
pub(crate) fn group_query(keys: &[&str]) -> String {
    let filter = keys
        .iter()
        .map(|k| format!("s.id STARTS WITH '{}#'", escape_str(k)))
        .collect::<Vec<_>>()
        .join(" OR ");
    format!("MATCH (s:Symbol) WHERE {filter} RETURN s.id, s.file, s.start_line, s.end_line")
}

/// The groups among `keys` from the rows [`group_query`] returned: a key whose
/// members are `key#<digits>` ids, two or more of them. An id is a member of
/// the key only through its own `#<digits>` suffix, so a member of another
/// owner, another file, or a deeper symbol (`key#1::inner`) never joins it.
pub(crate) fn groups_in(keys: &[&str], rows: &[Vec<String>]) -> HashMap<String, Live> {
    let mut members: HashMap<&str, Live> = HashMap::new();
    for row in rows {
        let [id, file, start, end] = &row[..] else {
            continue;
        };
        let (Ok(start), Ok(end)) = (start.parse::<usize>(), end.parse::<usize>()) else {
            continue;
        };
        let Some(key) = keys.iter().copied().find(|k| is_member_of(id, k)) else {
            continue;
        };
        members
            .entry(key)
            .or_insert_with(|| Live {
                file: file.clone(),
                spans: Vec::new(),
            })
            .spans
            .push((start, end));
    }
    members
        .into_iter()
        .filter(|(_, group)| group.spans.len() >= 2)
        .map(|(key, group)| (key.to_string(), group))
        .collect()
}

/// Whether `id` is `key#<digits>`: one ordinal member of `key`.
pub(crate) fn is_member_of(id: &str, key: &str) -> bool {
    id.strip_prefix(key)
        .and_then(|rest| rest.strip_prefix('#'))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
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
                "{key}  ->  {count} symbols have that name as `name#N` ids (an object literal \
                 nothing names, or an overload, is numbered this way); a bare key names them \
                 only when two or more share the name, and a `#N` id follows source order: do \
                 not key an entry on one"
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
/// keys name a file the graph has. A key with no symbol of its own resolves to
/// the group of same-named symbols it names, if there is one ([`groups_in`]).
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
                    live.insert(id.clone(), Live::single(file, start, end));
                }
            }
        }
        let unresolved: Vec<&str> = batch
            .iter()
            .copied()
            .filter(|k| !live.contains_key(*k))
            .collect();
        if !unresolved.is_empty() {
            let rows = rows_of(exec, &group_query(&unresolved));
            live.extend(groups_in(&unresolved, &rows));
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
    let graph_path = root
        .join(".infigraph")
        .join(infigraph_core::graph::names::STEM);
    let observed = ObservedGraph::open(root, &graph_path)?;
    observed.with_exec(|exec| resolve(exec, keys).0)
}

/// A symbol of a file, as `missing_docs` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileSymbol {
    pub id: String,
    pub kind: String,
    pub start: usize,
    pub end: usize,
    /// `None` when the extractor stored none (the column is empty).
    pub visibility: Option<String>,
}

/// Every symbol of each of `files`, as an observer reads the graph. Unlike
/// the resolution above, a failed read is an error: "no symbols" must not
/// read as "nothing to document".
pub(crate) fn symbols_in_files(
    root: &Path,
    files: &[&str],
) -> Result<HashMap<String, Vec<FileSymbol>>> {
    let graph_path = infigraph_core::graph::names::live_of(root);
    let observed = ObservedGraph::open(root, &graph_path)?;
    observed.with_exec(|exec| {
        let mut out: HashMap<String, Vec<FileSymbol>> = HashMap::new();
        for batch in files.chunks(BATCH) {
            let list = batch
                .iter()
                .map(|f| format!("'{}'", escape_str(f)))
                .collect::<Vec<_>>()
                .join(", ");
            let rows = exec
                .query_rows(&format!(
                    "MATCH (s:Symbol) WHERE s.file IN [{list}] \
                     RETURN s.file, s.id, s.kind, s.start_line, s.end_line, s.visibility"
                ))
                .map_err(|e| anyhow::anyhow!("the graph could not be read: {e}"))?;
            for row in rows {
                if let [file, id, kind, start, end, visibility] = &row[..] {
                    if let (Ok(start), Ok(end)) = (start.parse(), end.parse()) {
                        out.entry(file.clone()).or_default().push(FileSymbol {
                            id: id.clone(),
                            kind: kind.clone(),
                            start,
                            end,
                            visibility: Some(visibility.clone()).filter(|v| !v.is_empty()),
                        });
                    }
                }
            }
        }
        Ok(out)
    })?
}

/// Whether `key` resolves the way `doctor` judges it: a symbol the graph has,
/// or a `<file>::module` key whose file the graph has.
pub(crate) fn key_is_live(root: &Path, key: &str) -> Result<bool> {
    let graph_path = root
        .join(".infigraph")
        .join(infigraph_core::graph::names::STEM);
    let observed = ObservedGraph::open(root, &graph_path)?;
    observed.with_exec(|exec| {
        let (live, modules) = resolve(exec, &[key]);
        live.contains_key(key) || modules.iter().any(|m| m == key)
    })
}

/// Every check of one project's document links; empty when the project has no
/// documents or no links to check. `docs_sync` adds the git-based sync check.
/// The keys that resolve to no live symbol and no module, each with why. Only
/// the first `hint_cap` are classified; the rest are `NoSuchSymbol` without a
/// look. The summary check caps this (`HINT_CAP`); the listing does not.
fn unresolved_keys(
    exec: &dyn QueryExec,
    keys: &[&str],
    hint_cap: usize,
) -> (HashMap<String, Live>, Vec<(String, Miss)>) {
    let (live, modules) = resolve(exec, keys);
    let mut unresolved: Vec<(String, Miss)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for key in keys {
        if live.contains_key(*key) || modules.iter().any(|m| m == key) || !seen.insert(*key) {
            continue;
        }
        let miss = if unresolved.len() < hint_cap {
            classify(exec, key)
        } else {
            Miss::NoSuchSymbol
        };
        unresolved.push((key.to_string(), miss));
    }
    (live, unresolved)
}

/// One place a key is written and does not resolve: what it names, where, and
/// why it links nothing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MissingKey {
    pub key: String,
    pub document: String,
    pub line: usize,
    pub reason: &'static str,
    pub hint: String,
}

/// Every key that resolves to no live symbol, one row per place it is written,
/// sorted by document and line. Unlike the summary check, every key is
/// classified. Empty when documents are not enabled.
pub fn missing_symbols(root: &Path) -> Result<Vec<MissingKey>> {
    if !docs_switch::docs_enabled(root) || !docs_switch::docs_store_path(root).exists() {
        return Ok(Vec::new());
    }
    let documents = with_docs(root, |q| q.code_link_rows(LinkKind::Documents))?;
    if documents.is_empty() {
        return Ok(Vec::new());
    }
    let graph_path = root
        .join(".infigraph")
        .join(infigraph_core::graph::names::STEM);
    let observed = ObservedGraph::open(root, &graph_path)?;
    let keys: Vec<&str> = documents.iter().map(|r| r.target.as_str()).collect();
    let (_, unresolved) = observed.with_exec(|exec| unresolved_keys(exec, &keys, usize::MAX))?;
    let misses: HashMap<&str, &Miss> = unresolved.iter().map(|(k, m)| (k.as_str(), m)).collect();
    let mut rows: Vec<MissingKey> = documents
        .iter()
        .filter_map(|r| {
            let miss = misses.get(r.target.as_str())?;
            Some(MissingKey {
                key: r.target.clone(),
                document: r.doc_file.clone(),
                line: r.line_start,
                reason: miss.reason(),
                hint: miss.hint(&r.target),
            })
        })
        .collect();
    rows.sort_by(|a, b| (&a.document, a.line, &a.key).cmp(&(&b.document, b.line, &b.key)));
    Ok(rows)
}

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

    let graph_path = root
        .join(".infigraph")
        .join(infigraph_core::graph::names::STEM);
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
    let outcome = observed.with_exec(|exec| unresolved_keys(exec, &keys, HINT_CAP));
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

    #[test]
    fn entries_with_a_label_count_toward_a_keys_duplicates() {
        let rows: Vec<CodeLinkRow> = crate::code_links::extract_markdown(
            "docs/a.md",
            "## `a.ts::f`\n\nPlain.\n\n## `a.ts::f` \u{2014} watchers\n\nMore.\n\n## `a.ts::g`\n\nOnce.\n",
        )
        .into_iter()
        .filter(|r| r.kind == LinkKind::Documents)
        .collect();
        let got = keys_result(
            Path::new("/p"),
            0,
            &[],
            &Scanned {
                documents: &rows,
                ..Default::default()
            },
        );
        assert!(
            got.message.contains("1 key(s) have more than one entry"),
            "{}",
            got.message
        );
        assert!(
            got.message
                .contains("a.ts::f  ->  docs/a.md:1, docs/a.md:5"),
            "{}",
            got.message
        );
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
    /// Runs `git` in `dir` with both dates set to `date`.
    fn git_at(dir: &Path, args: &[&str], date: &str) {
        let out = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
    }

    fn stale_repo() -> (tempfile::TempDir, Vec<(CodeLinkRow, Live)>) {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str], date: &str| git_at(dir.path(), args, date);
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
        let live = Live::single("code.py", 1, 2);
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

    /// Adds `(id, file, start, end)` symbols to `store`.
    fn insert_symbols(
        store: &infigraph_core::graph::GraphStore,
        rows: &[(&str, &str, usize, usize)],
    ) {
        let conn = store.connection().unwrap();
        for (id, file, start, end) in rows {
            conn.query(&format!(
                "CREATE (:Symbol {{id: '{id}', name: '{id}', kind: 'Variable', file: '{file}', \
                 start_line: {start}, end_line: {end}, signature_hash: '', language: 'typescript', \
                 visibility: '', parent: '', docstring: '', complexity: 0, parameters: '', \
                 return_type: ''}})"
            ))
            .unwrap();
        }
    }

    /// A graph of `(id, file, start, end)` symbols.
    fn symbols(
        rows: &[(&str, &str, usize, usize)],
    ) -> (tempfile::TempDir, infigraph_core::graph::GraphStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = infigraph_core::graph::GraphStore::open(
            &dir.path().join(infigraph_core::graph::names::STEM),
        )
        .unwrap();
        insert_symbols(&store, rows);
        (dir, store)
    }

    #[test]
    fn key_is_live_accepts_the_bare_key_of_a_group() {
        // `doc_entry` create and rekey ask this before writing an entry.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".infigraph");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let store = infigraph_core::graph::GraphStore::open(
                &dir.join(infigraph_core::graph::names::STEM),
            )
            .unwrap();
            insert_symbols(
                &store,
                &[
                    ("a.ts::Obj::k#1", "a.ts", 2, 3),
                    ("a.ts::Obj::k#2", "a.ts", 5, 6),
                ],
            );
        }
        assert!(key_is_live(root.path(), "a.ts::Obj::k").unwrap());
        assert!(!key_is_live(root.path(), "a.ts::Obj::gone").unwrap());
    }

    fn resolved(store: &infigraph_core::graph::GraphStore, keys: &[&str]) -> HashMap<String, Live> {
        let conn = store.connection().unwrap();
        resolve(&LocalExec::new(&conn), keys).0
    }

    #[test]
    fn doctor_stops_listing_a_key_that_names_a_group() {
        let (_dir, store) = symbols(&[
            ("a.ts::Obj::k#1", "a.ts", 2, 3),
            ("a.ts::Obj::k#2", "a.ts", 5, 6),
        ]);
        let conn = store.connection().unwrap();
        let (_, unresolved) = unresolved_keys(
            &LocalExec::new(&conn),
            &["a.ts::Obj::k", "a.ts::Obj::gone"],
            usize::MAX,
        );
        let keys: Vec<&str> = unresolved.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["a.ts::Obj::gone"]);
    }

    #[test]
    fn a_bare_key_names_the_group_of_its_members() {
        let (_dir, store) = symbols(&[
            ("a.ts::Obj::k#1", "a.ts", 2, 3),
            ("a.ts::Obj::k#2", "a.ts", 5, 6),
            ("a.ts::Obj::k#3", "a.ts", 8, 9),
        ]);
        let live = resolved(&store, &["a.ts::Obj::k"]);
        assert_eq!(live["a.ts::Obj::k"].file, "a.ts");
        assert_eq!(
            live["a.ts::Obj::k"].spans,
            vec![(2, 3), (5, 6), (8, 9)],
            "every member's span"
        );
    }

    #[test]
    fn an_exact_bare_symbol_wins_over_the_group_of_the_same_name() {
        let (_dir, store) = symbols(&[
            ("a.ts::Obj::k", "a.ts", 9, 9),
            ("a.ts::Obj::k#1", "a.ts", 2, 3),
            ("a.ts::Obj::k#2", "a.ts", 5, 6),
        ]);
        assert_eq!(
            resolved(&store, &["a.ts::Obj::k"])["a.ts::Obj::k"].spans,
            [(9, 9)]
        );
    }

    #[test]
    fn an_ordinal_key_still_selects_one_member() {
        let (_dir, store) = symbols(&[
            ("a.ts::Obj::k#1", "a.ts", 2, 3),
            ("a.ts::Obj::k#2", "a.ts", 5, 6),
        ]);
        assert_eq!(
            resolved(&store, &["a.ts::Obj::k#2"])["a.ts::Obj::k#2"].spans,
            [(5, 6)]
        );
    }

    #[test]
    fn groups_do_not_merge_across_owners_or_files() {
        let (_dir, store) = symbols(&[
            ("a.ts::A::k#1", "a.ts", 2, 3),
            ("a.ts::A::k#2", "a.ts", 5, 6),
            ("a.ts::B::k#1", "a.ts", 8, 8),
            ("a.ts::B::k#2", "a.ts", 10, 10),
            ("b.ts::A::k#1", "b.ts", 1, 1),
            ("b.ts::A::k#2", "b.ts", 2, 2),
        ]);
        assert_eq!(
            resolved(&store, &["a.ts::A::k"])["a.ts::A::k"].spans,
            [(2, 3), (5, 6)],
            "only the members of A in a.ts"
        );
    }

    #[test]
    fn a_nested_symbol_is_not_a_member_of_the_group() {
        let (_dir, store) = symbols(&[
            ("a.ts::Obj::k#1", "a.ts", 2, 3),
            ("a.ts::Obj::k#1::inner", "a.ts", 2, 2),
            ("a.ts::Obj::k#2", "a.ts", 5, 6),
        ]);
        assert_eq!(
            resolved(&store, &["a.ts::Obj::k"])["a.ts::Obj::k"].spans,
            [(2, 3), (5, 6)]
        );
    }

    #[test]
    fn one_member_is_not_a_group() {
        let (_dir, store) = symbols(&[("a.ts::X::k#1", "a.ts", 2, 3)]);
        assert!(!resolved(&store, &["a.ts::X::k"]).contains_key("a.ts::X::k"));
    }

    /// `code.py` with two members of one name, `f` at lines 1-2 and 3-4,
    /// committed on 2024-01-01; then only the second body changes, on 2024-06-01.
    fn group_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str], date: &str| git_at(dir.path(), args, date);
        git(&["init", "-q"], "2024-01-01T00:00:00Z");
        std::fs::write(dir.path().join("doc.md"), "### `code.py::f`\n\nWhy f.\n").unwrap();
        std::fs::write(
            dir.path().join("code.py"),
            "def f():\n    return 1\ndef f():\n    return 2\n",
        )
        .unwrap();
        git(&["add", "."], "2024-01-01T00:00:00Z");
        git(&["commit", "-q", "-m", "one"], "2024-01-01T00:00:00Z");
        std::fs::write(
            dir.path().join("code.py"),
            "def f():\n    return 1\ndef f():\n    return 3\n",
        )
        .unwrap();
        git(&["commit", "-q", "-am", "two"], "2024-06-01T00:00:00Z");
        dir
    }

    #[test]
    fn a_group_is_stale_when_any_member_changed_not_only_the_first() {
        let dir = group_repo();
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
        let state = |live: Live| {
            judge(
                dir.path(),
                &[(row.clone(), live)],
                Instant::now(),
                SYNC_BUDGET,
            )
            .judged[0]
                .state
                .clone()
        };
        assert!(
            matches!(
                state(Live::single("code.py", 1, 2)),
                infigraph_core::git_blame::SyncState::InSync
            ),
            "the first member alone is unchanged since the entry"
        );
        let grouped = Live {
            file: "code.py".into(),
            spans: vec![(1, 2), (3, 4)],
        };
        assert!(
            matches!(
                state(grouped),
                infigraph_core::git_blame::SyncState::PossiblyStale { .. }
            ),
            "the second member changed after the entry"
        );
    }
}
