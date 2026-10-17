//! Which glossary entries describe code that changed after they did, and what
//! changed, so an agent knows what to edit in the markdown.
//!
//! This is the one judgement. `get_doc_context` labels a single entry and
//! `doctor --docs-sync` counts, both through [`verdict`], and the `stale_docs`
//! tool and command list with [`stale_docs`]; none keeps its own. The tool is read-only: infigraph never
//! writes prose, so the report says where the entry is and what the code did,
//! and the markdown is edited by whoever reads it. The document index then
//! refreshes through `reindex_docs` or the doc watcher as it does for any edit.
//!
//! Every git question goes through `infigraph_core::git_blame`, which is the
//! one seam to replace if the history is ever kept in the graph.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use infigraph_core::degraded::{live, DegradedMode, DOC_SYNC_UNKNOWN};
use infigraph_core::docs_switch;
use infigraph_core::git_blame::{self, BlameError, Changes, RangeBlame, SyncState, GIT_TIMEOUT};
use infigraph_core::path_scope::PathScope;

use crate::code_links::{CodeLinkRow, LinkKind};
use crate::link_check::{self, Live};

/// Entries listed when the caller names no limit.
pub const DEFAULT_LIMIT: usize = 20;

/// How long the tool judges exactly before the rest is judged per file. Shorter
/// than `doctor --docs-sync`'s budget: a tool call should answer in seconds.
pub const TOOL_BUDGET: Duration = Duration::from_secs(30);

/// A whole-file blame is slower than the single range `get_doc_context` asks
/// for, so a bulk judgement allows each call more than [`GIT_TIMEOUT`].
pub(crate) const BULK_GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Said whenever coarse results are shown: measured on sittir, the per-file
/// comparison flagged 311 entries where the exact one flagged 821.
pub(crate) const COARSE_UNDER_REPORTS: &str =
    "coarse results compare whole files and under-report \
     (about 62% on the repository this was measured on)";

/// What one listed entry shows of its history. A list is skimmed, so it is
/// short; one symbol asked for by name is being worked on, so it gets more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub commits: usize,
    pub chars: usize,
}

impl Caps {
    pub const LIST: Caps = Caps {
        commits: 3,
        chars: 900,
    };
    pub const SYMBOL: Caps = Caps {
        commits: 10,
        chars: 6_000,
    };
}

/// One entry judged against its code.
pub(crate) struct Judged {
    pub key: String,
    pub doc_file: String,
    pub doc_lines: (usize, usize),
    pub live: Live,
    pub state: SyncState,
    /// From the lines' own blame, not from the files' last commits.
    pub exact: bool,
}

impl Judged {
    pub fn is_stale(&self) -> bool {
        self.state != SyncState::InSync
    }

    /// How far the code ran ahead of the entry, in seconds; 0 when in sync.
    fn gap(&self) -> i64 {
        match &self.state {
            SyncState::PossiblyStale { code, entry } => code.time - entry.time,
            SyncState::InSync => 0,
        }
    }
}

/// The stale ones, the biggest gap first (exact before coarse, then by key, so
/// the order is the same every run).
pub(crate) fn rank(judged: &[Judged]) -> Vec<&Judged> {
    let mut stale: Vec<&Judged> = judged.iter().filter(|j| j.is_stale()).collect();
    stale.sort_by(|a, b| {
        (b.gap(), b.exact)
            .cmp(&(a.gap(), a.exact))
            .then_with(|| a.key.cmp(&b.key))
    });
    stale
}

/// Whether the entry for `target` is current, given the newest change to its
/// code and to its text. The one place the rule lives: [`judge`] (the tool and
/// `doctor --docs-sync`) and `linked::judge_entries` (`get_doc_context`) both
/// ask it, so anything that decides an entry is current regardless of git
/// history belongs here. `target` is the entry's key, which such a decision
/// would be recorded against.
pub(crate) fn verdict(_target: &str, code: &RangeBlame, entry: &RangeBlame) -> SyncState {
    git_blame::compare(code, entry)
}

/// The outcome of [`judge`].
pub(crate) struct Judgement {
    pub judged: Vec<Judged>,
    /// Entries git gave no answer for, even per file.
    pub unknown: usize,
    /// The first reason git gave, for the report.
    pub first_error: Option<String>,
}

/// `file` -> the blame of each of its ranges, or why not.
type Blamed = HashMap<String, Result<Vec<RangeBlame>, String>>;

/// Judges each `(entry, symbol)` pair: exactly (one blame per file, documents
/// first) while `budget` lasts, then per file by last commit, which is cheap
/// and under-reports. A pair git cannot answer for is counted as unknown.
pub(crate) fn judge(
    root: &Path,
    entries: &[(CodeLinkRow, Live)],
    started: Instant,
    budget: Duration,
) -> Judgement {
    // The ranges each file is asked about, in one git call per file.
    let mut ranges: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
    for (row, live) in entries {
        ranges
            .entry(live.file.as_str())
            .or_default()
            .push((live.start, live.end));
        ranges
            .entry(row.doc_file.as_str())
            .or_default()
            .push((row.line_start, row.line_end));
    }
    // Documents first: there are few of them and every entry needs one.
    let doc_files: HashSet<&str> = entries.iter().map(|(r, _)| r.doc_file.as_str()).collect();
    let mut order: Vec<&str> = ranges.keys().copied().collect();
    order.sort_by_key(|f| (!doc_files.contains(f), *f));

    let mut exact: Blamed = HashMap::new();
    let mut first_error: Option<String> = None;
    for file in order {
        if started.elapsed() >= budget {
            continue;
        }
        let got = git_blame::blame_ranges(root, file, &ranges[file], BULK_GIT_TIMEOUT)
            .map_err(|e| e.to_string());
        if let Err(e) = &got {
            first_error.get_or_insert_with(|| e.clone());
        }
        exact.insert(file.to_string(), got);
    }

    let lookup = |file: &str, range: (usize, usize)| -> Option<RangeBlame> {
        let blamed = exact.get(file)?.as_ref().ok()?;
        let at = ranges[file].iter().position(|r| *r == range)?;
        blamed.get(at).cloned()
    };
    let mut coarse_cache: HashMap<String, Option<RangeBlame>> = HashMap::new();
    let mut coarse = |file: &str| -> Option<RangeBlame> {
        coarse_cache
            .entry(file.to_string())
            .or_insert_with(
                || match git_blame::last_commit_of_file(root, file, BULK_GIT_TIMEOUT) {
                    Ok(found) => found,
                    Err(e) => {
                        first_error.get_or_insert_with(|| e.to_string());
                        None
                    }
                },
            )
            .clone()
    };

    let mut judged = Vec::new();
    let mut unknown = 0usize;
    for (row, live) in entries {
        let exact_pair = lookup(&live.file, (live.start, live.end))
            .zip(lookup(&row.doc_file, (row.line_start, row.line_end)));
        let (pair, is_exact) = match exact_pair {
            Some(pair) => (Some(pair), true),
            None => (coarse(&live.file).zip(coarse(&row.doc_file)), false),
        };
        match pair {
            Some((code, entry)) => judged.push(Judged {
                key: row.target.clone(),
                doc_file: row.doc_file.clone(),
                doc_lines: (row.line_start, row.line_end),
                live: live.clone(),
                state: verdict(&row.target, &code, &entry),
                exact: is_exact,
            }),
            None => unknown += 1,
        }
    }
    Judgement {
        judged,
        unknown,
        first_error,
    }
}

/// What to list.
#[derive(Debug, Clone)]
pub struct Options {
    /// Only entries whose symbol's file, or whose document, lies under it.
    pub scope: PathScope,
    /// Only the entry keyed to this symbol id.
    pub symbol: Option<String>,
    /// At most this many entries, the biggest gap first.
    pub limit: usize,
    /// How long to judge exactly before judging per file.
    pub budget: Duration,
    pub caps: Caps,
}

impl Options {
    /// The tool's defaults: a named symbol gets the fuller history.
    pub fn new(scope: PathScope, symbol: Option<String>, limit: Option<usize>) -> Self {
        let caps = if symbol.is_some() {
            Caps::SYMBOL
        } else {
            Caps::LIST
        };
        Self {
            scope,
            symbol,
            limit: limit.unwrap_or(DEFAULT_LIMIT).max(1),
            budget: TOOL_BUDGET,
            caps,
        }
    }
}

/// One listed entry.
#[derive(Debug, Clone)]
pub struct StaleEntry {
    pub symbol: String,
    pub doc_file: String,
    pub doc_lines: (usize, usize),
    pub code_file: String,
    pub code_lines: (usize, usize),
    pub state: SyncState,
    pub exact: bool,
    /// What changed since the entry, or why git could not say.
    pub changes: Result<Changes, String>,
}

/// The answer, with every way it can be partial named.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// The project has no document index to judge (or it cannot be read).
    pub unavailable: Option<String>,
    /// Entries in scope whose symbol is in the graph.
    pub entries: usize,
    /// How many of them are possibly stale.
    pub stale: usize,
    /// Of `entries`, judged by the lines' own blame / by file (under-reports).
    pub exact: usize,
    pub coarse: usize,
    pub stale_coarse: usize,
    /// Entries git gave no answer for.
    pub unknown: usize,
    pub sync_unknown: Option<String>,
    /// The ranked entries up to the limit.
    pub shown: Vec<StaleEntry>,
    pub notes: Vec<String>,
    pub caps: Option<Caps>,
}

fn no_index() -> Report {
    Report {
        unavailable: Some(
            "this project has no document index: run `infigraph index-docs` (and glossary \
             entries keyed to symbols appear here when their code changes)"
                .to_string(),
        ),
        ..Report::default()
    }
}

/// The code file an entry key names: the part before the first `::`.
fn code_file_of(key: &str) -> &str {
    key.split_once("::").map_or(key, |(file, _)| file)
}

/// Which entries are possibly stale in the project at `root`, and what changed
/// in their code since. Never writes, never starts a daemon, and never waits
/// longer than `options.budget` for the bulk judgement or [`GIT_TIMEOUT`] for
/// one entry's history.
pub fn stale_docs(root: &Path, options: &Options) -> Report {
    if !docs_switch::docs_enabled(root) || !docs_switch::docs_store_path(root).exists() {
        return no_index();
    }
    let started = Instant::now();
    let mut report = Report {
        caps: Some(options.caps),
        ..Report::default()
    };

    let rows = match link_check::with_docs(root, |q| q.code_link_rows(LinkKind::Documents)) {
        Ok(rows) => rows,
        Err(e) => {
            report.unavailable = Some(format!("the document store could not be read: {e:#}"));
            return report;
        }
    };
    let in_scope = |row: &CodeLinkRow| {
        options.symbol.as_deref().map_or_else(
            || {
                options.scope.contains(code_file_of(&row.target))
                    || options.scope.contains(&row.doc_file)
            },
            |symbol| row.target == symbol,
        )
    };
    let wanted: Vec<CodeLinkRow> = rows.into_iter().filter(|r| in_scope(r)).collect();
    if let Some(symbol) = &options.symbol {
        if wanted.is_empty() {
            report
                .notes
                .push(format!("no glossary entry is keyed to `{symbol}`"));
        }
    }

    let keys: Vec<&str> = wanted.iter().map(|r| r.target.as_str()).collect();
    let live_symbols = match link_check::live_symbols(root, &keys) {
        Ok(live) => live,
        Err(e) => {
            report.unavailable = Some(format!("the graph could not be read: {e:#}"));
            return report;
        }
    };
    let unresolved = wanted
        .iter()
        .filter(|r| !live_symbols.contains_key(&r.target))
        .count();
    if unresolved > 0 {
        report.notes.push(format!(
            "{unresolved} entries are keyed to a symbol the graph does not have and cannot be \
             judged (`infigraph doctor` lists them)"
        ));
    }
    let entries: Vec<(CodeLinkRow, Live)> = wanted
        .into_iter()
        .filter_map(|row| {
            let live = live_symbols.get(&row.target)?.clone();
            Some((row, live))
        })
        .collect();
    report.entries = entries.len();

    let Judgement {
        judged,
        unknown,
        first_error,
    } = judge(root, &entries, started, options.budget);
    report.unknown = unknown;
    report.exact = judged.iter().filter(|j| j.exact).count();
    report.coarse = judged.len() - report.exact;
    report.stale = judged.iter().filter(|j| j.is_stale()).count();
    report.stale_coarse = judged.iter().filter(|j| j.is_stale() && !j.exact).count();
    if judged.is_empty() && unknown > 0 {
        let reason = first_error.unwrap_or_else(|| "git gave no answer".to_string());
        live::set(
            root,
            DegradedMode::DocSyncUnknown {
                reason: reason.clone(),
            },
        );
        report.sync_unknown = Some(reason);
    } else {
        live::clear(root, DOC_SYNC_UNKNOWN);
    }

    let mut git_gave_up = false;
    for j in rank(&judged).into_iter().take(options.limit) {
        let SyncState::PossiblyStale { entry, .. } = &j.state else {
            continue;
        };
        let changes = if git_gave_up {
            Err("not fetched: git timed out on an earlier entry".to_string())
        } else {
            git_blame::changes_since(
                root,
                &j.live.file,
                (j.live.start, j.live.end),
                &entry.commit,
                options.caps.commits,
                GIT_TIMEOUT,
            )
            .map_err(|e| {
                if e == BlameError::TimedOut {
                    git_gave_up = true;
                }
                e.to_string()
            })
        };
        report.shown.push(StaleEntry {
            symbol: j.key.clone(),
            doc_file: j.doc_file.clone(),
            doc_lines: j.doc_lines,
            code_file: j.live.file.clone(),
            code_lines: (j.live.start, j.live.end),
            state: j.state.clone(),
            exact: j.exact,
            changes,
        });
    }
    report
}

/// Appends `text` line by line to `out` (indented) while `budget` chars last;
/// false when some of it was left out.
fn push_capped(out: &mut String, text: &str, indent: &str, budget: &mut usize) -> bool {
    for line in text.lines() {
        let cost = indent.len() + line.len() + 1;
        if cost > *budget {
            *budget = 0;
            return false;
        }
        *budget -= cost;
        out.push_str(indent);
        out.push_str(line);
        out.push('\n');
    }
    true
}

impl StaleEntry {
    fn render(&self, number: usize, caps: Caps, out: &mut String) {
        out.push_str(&format!(
            "\n{number}. {} -- entry {}:{}-{}\n",
            self.symbol, self.doc_file, self.doc_lines.0, self.doc_lines.1
        ));
        let label = self.state.label().unwrap_or_default();
        out.push_str(&format!(
            "   {label} (code at {}:{}-{}){}\n",
            self.code_file,
            self.code_lines.0,
            self.code_lines.1,
            if self.exact { "" } else { " [coarse]" }
        ));
        let changes = match &self.changes {
            Ok(changes) => changes,
            Err(why) => {
                out.push_str(&format!("   what changed: unknown ({why})\n"));
                return;
            }
        };
        out.push_str("   what changed since the entry:\n");
        let mut budget = caps.chars;
        let mut complete = true;
        if let Some(patch) = &changes.uncommitted {
            out.push_str("   uncommitted:\n");
            complete &= push_capped(out, patch, "     ", &mut budget);
        }
        for change in &changes.commits {
            out.push_str(&format!(
                "   {} {} {}\n",
                &change.commit[..change.commit.len().min(8)],
                infigraph_core::git_blame::civil_date(change.time),
                change.subject
            ));
            if budget > 0 {
                complete &= push_capped(out, &change.patch, "     ", &mut budget);
            } else if !change.patch.is_empty() {
                complete = false;
            }
        }
        if changes.commits.is_empty() && changes.uncommitted.is_none() {
            out.push_str("   (git shows no commit touching these lines after the entry's)\n");
        }
        if changes.more_commits {
            out.push_str("   (truncated: older commits not shown)\n");
        }
        if !complete {
            out.push_str(&format!(
                "   (truncated: patch cut at {} chars)\n",
                caps.chars
            ));
        }
    }
}

impl Report {
    /// The text the tool and the command both print.
    pub fn render(&self) -> String {
        if let Some(why) = &self.unavailable {
            return format!("Stale docs: {why}\n");
        }
        let caps = self.caps.unwrap_or(Caps::LIST);
        let mut out = format!(
            "Stale docs: {} of {} entries possibly stale (exact git blame of {} entries)",
            self.stale - self.stale_coarse,
            self.exact,
            self.exact
        );
        if self.shown.len() < self.stale {
            out.push_str(&format!(
                "; showing the {} with the biggest gap between code and entry",
                self.shown.len()
            ));
        }
        out.push('\n');
        if self.coarse > 0 {
            out.push_str(&format!(
                "plus {} of {} entries judged per file because the bulk blame ran out of time or \
                 failed on their files ({COARSE_UNDER_REPORTS})\n",
                self.stale_coarse, self.coarse
            ));
        }
        if self.unknown > 0 {
            out.push_str(&format!(
                "{} entries could not be judged: {}\n",
                self.unknown,
                self.sync_unknown
                    .as_deref()
                    .unwrap_or("git gave no answer for their files")
            ));
        }
        for note in &self.notes {
            out.push_str(&format!("{note}\n"));
        }
        if self.entries > 0 && self.stale == 0 && self.unknown == 0 {
            out.push_str("Every entry is as new as its code (or newer): none is possibly stale.\n");
        }
        for (i, entry) in self.shown.iter().enumerate() {
            entry.render(i + 1, caps, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use infigraph_core::git_blame::Change;

    fn at(commit: &str, time: i64) -> RangeBlame {
        RangeBlame {
            commit: commit.repeat(40)[..40].to_string(),
            time,
            dirty: false,
        }
    }

    fn entry(changes: Changes) -> StaleEntry {
        StaleEntry {
            symbol: "src/a.py::f".to_string(),
            doc_file: "docs/g.md".to_string(),
            doc_lines: (3, 9),
            code_file: "src/a.py".to_string(),
            code_lines: (10, 20),
            state: SyncState::PossiblyStale {
                code: at("b", 1_760_140_800),
                entry: at("a", 1_758_758_400),
            },
            exact: true,
            changes: Ok(changes),
        }
    }

    fn change(subject: &str, patch: &str) -> Change {
        Change {
            commit: "c".repeat(40),
            time: 1_760_140_800,
            subject: subject.to_string(),
            patch: patch.to_string(),
        }
    }

    fn rendered(e: &StaleEntry, caps: Caps) -> String {
        let mut out = String::new();
        e.render(1, caps, &mut out);
        out
    }

    #[test]
    fn a_patch_longer_than_the_char_cap_is_cut_at_a_line_and_marked() {
        let patch: String = (0..50).map(|i| format!("+line number {i}\n")).collect();
        let e = entry(Changes {
            commits: vec![change("big edit", &patch)],
            ..Changes::default()
        });
        let out = rendered(
            &e,
            Caps {
                commits: 3,
                chars: 200,
            },
        );
        assert!(out.contains("big edit"), "{out}");
        assert!(out.contains("+line number 0"), "{out}");
        assert!(!out.contains("+line number 49"), "{out}");
        assert!(out.contains("(truncated: patch cut at 200 chars)"), "{out}");
    }

    #[test]
    fn a_patch_inside_the_cap_is_whole_and_unmarked() {
        let e = entry(Changes {
            commits: vec![change("small edit", "-old\n+new\n")],
            ..Changes::default()
        });
        let out = rendered(&e, Caps::LIST);
        assert!(out.contains("-old") && out.contains("+new"), "{out}");
        assert!(!out.contains("truncated"), "{out}");
    }

    #[test]
    fn older_commits_not_shown_are_marked_and_uncommitted_edits_come_first() {
        let e = entry(Changes {
            commits: vec![change("newest", "+x\n")],
            more_commits: true,
            uncommitted: Some("+draft\n".to_string()),
        });
        let out = rendered(&e, Caps::LIST);
        assert!(
            out.contains("(truncated: older commits not shown)"),
            "{out}"
        );
        assert!(
            out.find("uncommitted:").unwrap() < out.find("newest").unwrap(),
            "{out}"
        );
    }

    #[test]
    fn history_git_could_not_give_is_said_not_left_empty() {
        let mut e = entry(Changes::default());
        e.changes = Err("git did not answer in time".to_string());
        let out = rendered(&e, Caps::LIST);
        assert!(
            out.contains("what changed: unknown (git did not answer in time)"),
            "{out}"
        );
    }

    #[test]
    fn the_verdict_is_possibly_stale_only_when_the_code_is_newer_than_the_entry() {
        let (old, new) = (at("a", 100), at("b", 200));
        assert!(matches!(
            verdict("src/a.py::f", &new, &old),
            SyncState::PossiblyStale { .. }
        ));
        assert_eq!(verdict("src/a.py::f", &old, &new), SyncState::InSync);
        assert_eq!(verdict("src/a.py::f", &new, &new), SyncState::InSync);
    }

    #[test]
    fn the_biggest_gap_ranks_first_and_in_sync_entries_are_left_out() {
        let judged = |key: &str, code_time: i64, entry_time: i64| Judged {
            key: key.to_string(),
            doc_file: "d.md".to_string(),
            doc_lines: (1, 2),
            live: Live {
                file: "a.py".to_string(),
                start: 1,
                end: 2,
            },
            state: git_blame::compare(&at("b", code_time), &at("a", entry_time)),
            exact: true,
        };
        let all = [
            judged("small", 200, 100),
            judged("fresh", 100, 200),
            judged("large", 900, 100),
        ];
        let keys: Vec<&str> = rank(&all).iter().map(|j| j.key.as_str()).collect();
        assert_eq!(keys, ["large", "small"]);
    }
}
