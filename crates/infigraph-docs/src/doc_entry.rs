//! `doc_entry`: the mechanical part of keeping a glossary entry in step with
//! its code. The caller supplies the prose; this finds the entry, edits
//! exactly its lines, and refreshes the index. infigraph never writes prose:
//! with no text given there is nothing it can do.
//!
//! Every operation plans first and writes only when asked (`apply`). The
//! plan reads the file as it lies now and finds the entry with the same scan
//! the indexer stores ([`code_links::entries`]); the stored rows only say
//! which file to look in, so a watcher that lags cannot make an edit land on
//! the wrong lines. A plan that would change which entries a file has (text
//! carrying a key heading of its own) is refused: that is `create`, `rekey` or
//! `delete`. Each operation is idempotent: asked again, it says it is done.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use infigraph_core::docs_switch;
use infigraph_core::git_blame::civil_date;

use crate::ack::{self, AckPlan};
use crate::code_links::{entries, Entry, LinkKind};
use crate::link_check;
use crate::ops;

/// One operation on the entries of a project's glossary.
#[derive(Debug, Clone)]
pub enum Op {
    /// Replace the entry's lead text (or the block under one of its
    /// ride-along sub-headings) with `text`.
    Replace {
        target: String,
        text: String,
        section: Option<String>,
    },
    /// Change `old` to `new` inside the entry, where `old` occurs once (or,
    /// with `replace_all`, however often, but only inside the entry).
    Edit {
        target: String,
        old: String,
        new: String,
        replace_all: bool,
    },
    /// Add an entry for a symbol that has none.
    Create {
        target: String,
        text: String,
        doc_file: Option<String>,
    },
    /// Point an entry at another key (a renamed or moved symbol), body
    /// untouched. `ack` also records the entry as reviewed against it.
    Rekey {
        target: String,
        new_target: String,
        force: bool,
        ack: bool,
    },
    /// Remove an entry whose symbol is gone.
    Delete { target: String, force: bool },
    /// Record that entries were read against their symbols as they are now.
    Ack { targets: Vec<String> },
}

impl Op {
    fn name(&self) -> &'static str {
        match self {
            Op::Replace { .. } => "replace",
            Op::Edit { .. } => "edit",
            Op::Create { .. } => "create",
            Op::Rekey { .. } => "rekey",
            Op::Delete { .. } => "delete",
            Op::Ack { .. } => "ack",
        }
    }

    fn subject(&self) -> String {
        match self {
            Op::Replace { target, .. }
            | Op::Edit { target, .. }
            | Op::Create { target, .. }
            | Op::Delete { target, .. } => target.clone(),
            Op::Rekey {
                target, new_target, ..
            } => format!("{target} -> {new_target}"),
            Op::Ack { targets } => targets.join(", "),
        }
    }
}

/// A file's new text.
struct Change {
    file: String,
    before: String,
    after: String,
}

/// What happens after the file is written.
enum Then {
    Nothing,
    /// Forget the reviews of this target (its entry is gone).
    DropAck(String),
    /// Review this target's entry against its symbol.
    Ack(String),
}

enum Plan {
    /// Nothing to do, and why.
    Done(String),
    Write {
        change: Change,
        notes: Vec<String>,
        then: Then,
    },
    Review(AckPlan),
}

/// Plans `op` and, with `apply`, carries it out. Returns what was (or would
/// be) done. A refusal is an error that says why.
pub fn run(root: &Path, op: &Op, apply: bool) -> Result<String> {
    let name = op.name();
    let head = format!("doc_entry {name} {}", op.subject());
    match plan(root, op)? {
        Plan::Done(why) => Ok(format!("{head}: already done ({why})")),
        Plan::Review(review) => finish_review(root, &head, review, apply),
        Plan::Write {
            change,
            notes,
            then,
        } => {
            let mut out = if apply {
                format!("{head}: applied to {}\n", change.file)
            } else {
                format!(
                    "{head}: dry run, nothing written (pass apply=true to write {})\n",
                    change.file
                )
            };
            out.push_str(&diff(&change.file, &change.before, &change.after));
            for note in &notes {
                out.push_str(&format!("{note}\n"));
            }
            if apply {
                write_file(root, &change)?;
                out.push_str(&after_write(root, then));
            } else if let Then::Ack(target) = &then {
                out.push_str(&format!(
                    "would also record `{target}` as reviewed against its symbol\n"
                ));
            }
            Ok(out)
        }
    }
}

fn plan(root: &Path, op: &Op) -> Result<Plan> {
    if let Op::Ack { targets } = op {
        return Ok(Plan::Review(ack::plan(root, targets)?));
    }
    if !docs_switch::docs_enabled(root) || !docs_switch::docs_store_path(root).exists() {
        bail!("this project has no document index: run `infigraph index-docs` first");
    }
    match op {
        Op::Replace {
            target,
            text,
            section,
        } => plan_replace(root, target, text, section.as_deref()),
        Op::Edit {
            target,
            old,
            new,
            replace_all,
        } => plan_edit(root, target, old, new, *replace_all),
        Op::Create {
            target,
            text,
            doc_file,
        } => plan_create(root, target, text, doc_file.as_deref()),
        Op::Rekey {
            target,
            new_target,
            force,
            ack,
        } => plan_rekey(root, target, new_target, *force, *ack),
        Op::Delete { target, force } => plan_delete(root, target, *force),
        Op::Ack { .. } => unreachable!("handled above"),
    }
}

// ---- reading an entry ---------------------------------------------------

/// A document file, and the entry for one key as it lies in it now.
struct Located {
    file: String,
    text: String,
    entry: Entry,
}

fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn split_lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

fn join_lines(lines: &[String], like: &str) -> String {
    let eol = eol_of(like);
    let mut out = lines.join(eol);
    if like.ends_with('\n') || like.is_empty() {
        out.push_str(eol);
    }
    out
}

/// A document file under `root`, by the relative id the index stores. Refuses
/// a path that leaves the project.
fn doc_path(root: &Path, file: &str) -> Result<PathBuf> {
    let rel = Path::new(file);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        bail!("`{file}` is not a path inside the project");
    }
    Ok(root.join(rel))
}

fn read_doc(root: &Path, file: &str) -> Result<String> {
    std::fs::read_to_string(doc_path(root, file)?)
        .with_context(|| format!("cannot read the document `{file}`"))
}

/// Where the stored index says the entry for `target` is: `(file, line)` per
/// row.
fn indexed_at(root: &Path, target: &str) -> Result<Vec<(String, usize)>> {
    let keys = [target.to_string()];
    let rows = link_check::with_docs(root, |q| q.code_links_for(LinkKind::Documents, &keys))?;
    Ok(rows
        .into_iter()
        .map(|r| (r.doc_file, r.line_start))
        .collect())
}

/// The entry for `target`, read from its file as it is now. `None` when the
/// index knows none. An index that has fallen behind the file is an error.
fn locate(root: &Path, target: &str) -> Result<Option<Located>> {
    let mut at = indexed_at(root, target)?;
    at.sort();
    at.dedup_by(|a, b| a.0 == b.0);
    let file = match at.as_slice() {
        [] => return Ok(None),
        [(file, _)] => file.clone(),
        many => bail!(
            "`{target}` has an entry in more than one document ({}); remove one first",
            many.iter()
                .map(|(f, l)| format!("{f}:{l}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let text = read_doc(root, &file)?;
    let mut found = entries(&text)
        .into_iter()
        .filter(|e| e.key == target)
        .collect::<Vec<_>>();
    match found.len() {
        0 => bail!(
            "the index says `{target}` is in {file}, but the file has no such entry now: refresh \
             the index (`infigraph index-docs`) and ask again"
        ),
        1 => Ok(Some(Located {
            file,
            text,
            entry: found.remove(0),
        })),
        n => bail!("{file} has {n} entries keyed `{target}`; remove the duplicates first"),
    }
}

fn require(root: &Path, target: &str) -> Result<Located> {
    locate(root, target)?
        .with_context(|| format!("no glossary entry is keyed to `{target}` (create one first)"))
}

fn symbol_is_live(root: &Path, target: &str) -> Result<bool> {
    Ok(link_check::live_symbols(root, &[target])?.contains_key(target))
}

// ---- editing lines ------------------------------------------------------

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// The entry keys of `text`, in order: what an edit must not change.
fn keys_of(text: &str) -> Vec<String> {
    entries(text).into_iter().map(|e| e.key).collect()
}

fn refuse_structural_change(before: &str, after: &str) -> Result<()> {
    if keys_of(before) != keys_of(after) {
        bail!(
            "the new text would change which entries the file has (it carries a key heading): \
             use create, rekey or delete for that"
        );
    }
    Ok(())
}

/// Splits `text` into the lines of a body.
fn body_lines(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    text.trim_matches(|c| c == '\n' || c == '\r')
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// The 1-based, inclusive line region a `replace` targets: the entry's lead
/// text, or the block under the sub-heading `section`.
fn replace_region(entry: &Entry, section: Option<&str>) -> Result<(usize, usize)> {
    let Some(name) = section else {
        let end = entry.subs.first().map_or(entry.end, |s| s.line - 1);
        return Ok((entry.start + 1, end));
    };
    let wanted = name.trim().trim_start_matches('#').trim();
    let Some(at) = entry
        .subs
        .iter()
        .position(|s| s.text.trim().eq_ignore_ascii_case(wanted))
    else {
        let have: Vec<&str> = entry.subs.iter().map(|s| s.text.as_str()).collect();
        bail!(
            "`{}` has no sub-heading `{wanted}` (it has: {})",
            entry.key,
            if have.is_empty() {
                "none".to_string()
            } else {
                have.join(", ")
            }
        );
    };
    let sub = &entry.subs[at];
    let end = entry.subs[at + 1..]
        .iter()
        .find(|s| s.level <= sub.level)
        .map_or(entry.end, |s| s.line - 1);
    Ok((sub.line + 1, end))
}

fn plan_replace(root: &Path, target: &str, text: &str, section: Option<&str>) -> Result<Plan> {
    if body_lines(text).is_empty() {
        bail!("replace needs `text`: infigraph does not write the prose");
    }
    let located = require(root, target)?;
    let (r0, r1) = replace_region(&located.entry, section)?;
    let mut lines = split_lines(&located.text);
    let new = body_lines(text);
    let content: Vec<usize> = (r0..=r1)
        .filter(|n| *n <= lines.len() && !is_blank(&lines[*n - 1]))
        .collect();
    match (content.first(), content.last()) {
        (Some(&a), Some(&b)) => {
            if lines[a - 1..b]
                .iter()
                .map(|l| l.trim_end())
                .eq(new.iter().map(String::as_str))
            {
                return Ok(Plan::Done("the entry already has that text".to_string()));
            }
            lines.splice(a - 1..b, new);
        }
        _ => {
            // Nothing there yet: put the text a blank line below the heading.
            let at = r0 - 1;
            let mut put = vec![String::new()];
            put.extend(new);
            if lines.get(at).is_some_and(|l| !is_blank(l)) {
                put.push(String::new());
            }
            lines.splice(at..at, put);
        }
    }
    let after = join_lines(&lines, &located.text);
    refuse_structural_change(&located.text, &after)?;
    Ok(Plan::Write {
        change: Change {
            file: located.file,
            before: located.text,
            after,
        },
        notes: Vec::new(),
        then: Then::Nothing,
    })
}

fn plan_edit(root: &Path, target: &str, old: &str, new: &str, replace_all: bool) -> Result<Plan> {
    if old.is_empty() {
        bail!("edit needs a non-empty `old`");
    }
    let located = require(root, target)?;
    let mut lines = split_lines(&located.text);
    let (first, last) = (located.entry.start + 1, located.entry.end);
    let region = lines[first - 1..last].join("\n");
    let count = region.matches(old).count();
    if count == 0 {
        if !new.is_empty() && region.contains(new) {
            return Ok(Plan::Done("the entry already has the new text".to_string()));
        }
        let elsewhere = entries(&located.text)
            .into_iter()
            .filter(|e| e.key != target)
            .find(|e| lines[e.start - 1..e.end].join("\n").contains(old));
        bail!(match elsewhere {
            Some(e) => format!(
                "`old` is not in the entry for `{target}`; it is outside it, in the entry for \
                 `{}` (line {})",
                e.key, e.start
            ),
            None if located.text.contains(old) => format!(
                "`old` is not in the entry for `{target}`; it is outside it (not in any entry)"
            ),
            None => format!("`old` was not found in the entry for `{target}`"),
        });
    }
    if count > 1 && !replace_all {
        bail!(
            "`old` has {count} matches in the entry for `{target}`: make it unique, or pass \
             replace_all to change them all (only inside this entry)"
        );
    }
    let changed = if replace_all {
        region.replace(old, new)
    } else {
        region.replacen(old, new, 1)
    };
    lines.splice(first - 1..last, changed.split('\n').map(str::to_string));
    let after = join_lines(&lines, &located.text);
    refuse_structural_change(&located.text, &after)?;
    Ok(Plan::Write {
        change: Change {
            file: located.file,
            before: located.text,
            after,
        },
        notes: Vec::new(),
        then: Then::Nothing,
    })
}

// ---- create -------------------------------------------------------------

/// `src/app.py` of `src/app.py::Cart::add`.
fn code_file_of(key: &str) -> &str {
    key.split("::").next().unwrap_or(key)
}

fn parent_dir(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(dir, _)| dir)
}

/// The document with the most entries among `rows`, or why there is no single
/// answer.
fn majority(counts: &BTreeMap<&str, usize>) -> Result<Option<String>> {
    let Some(&best) = counts.values().max() else {
        return Ok(None);
    };
    let top: Vec<&str> = counts
        .iter()
        .filter(|(_, n)| **n == best)
        .map(|(f, _)| *f)
        .collect();
    match top.as_slice() {
        [one] => Ok(Some((*one).to_string())),
        many => bail!(
            "these documents hold equally many neighbouring entries: {}; pass doc_file to choose",
            many.join(", ")
        ),
    }
}

/// The document a new entry for `target` belongs in: the one already holding
/// most entries for the same code file, else most entries below the nearest
/// directory that has any (never the project root: that is every document).
fn choose_doc(root: &Path, target: &str, explicit: Option<&str>) -> Result<String> {
    if let Some(file) = explicit {
        let path = doc_path(root, file)?;
        if !path.is_file() {
            bail!(
                "doc_file `{file}` does not exist (a new document is not invented here; create \
                 the file first)"
            );
        }
        return Ok(file.to_string());
    }
    let rows = link_check::with_docs(root, |q| q.code_link_rows(LinkKind::Documents))?;
    let code_file = code_file_of(target);
    let mut same: BTreeMap<&str, usize> = BTreeMap::new();
    for row in rows.iter().filter(|r| code_file_of(&r.target) == code_file) {
        *same.entry(row.doc_file.as_str()).or_default() += 1;
    }
    if let Some(found) = majority(&same)? {
        return Ok(found);
    }
    let mut dir = parent_dir(code_file);
    while let Some(d) = dir {
        let prefix = format!("{d}/");
        let mut below: BTreeMap<&str, usize> = BTreeMap::new();
        for row in rows
            .iter()
            .filter(|r| code_file_of(&r.target).starts_with(&prefix))
        {
            *below.entry(row.doc_file.as_str()).or_default() += 1;
        }
        if let Some(found) = majority(&below)? {
            return Ok(found);
        }
        dir = parent_dir(d);
    }
    bail!(
        "no glossary document is known for `{code_file}` or the directories above it: pass \
         doc_file to say where the entry goes"
    )
}

fn plan_create(root: &Path, target: &str, text: &str, explicit_doc: Option<&str>) -> Result<Plan> {
    if !target.contains("::") {
        bail!("`{target}` is not a symbol id (`file::qualified::name`)");
    }
    let body = body_lines(text);
    if body.is_empty() {
        bail!("create needs `text`: infigraph does not write the prose");
    }
    if let Some((file, line)) = indexed_at(root, target)?.first() {
        bail!("`{target}` already has an entry in {file}:{line}");
    }
    if !symbol_is_live(root, target)? {
        bail!("`{target}` is not in the graph: an entry needs a symbol it can resolve to");
    }
    let file = choose_doc(root, target, explicit_doc)?;
    let before = read_doc(root, &file)?;
    let present = entries(&before);
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for e in &present {
        *counts.entry(e.level).or_default() += 1;
    }
    let level = counts
        .iter()
        .max_by_key(|(level, n)| (**n, std::cmp::Reverse(**level)))
        .map_or(3, |(level, _)| *level);
    let mut lines = split_lines(&before);
    while lines.last().is_some_and(|l| is_blank(l)) {
        lines.pop();
    }
    let code_file = code_file_of(target);
    let after_line = present
        .iter()
        .filter(|e| code_file_of(&e.key) == code_file)
        .map(|e| e.end)
        .max()
        .unwrap_or(lines.len())
        .min(lines.len());
    let mut put = vec![
        String::new(),
        format!("{} `{target}`", "#".repeat(level)),
        String::new(),
    ];
    put.extend(body);
    lines.splice(after_line..after_line, put);
    let mut after = join_lines(&lines, &before);
    if !after.ends_with('\n') {
        after.push_str(eol_of(&before));
    }
    refuse_structural_change_added(&before, &after, target)?;
    Ok(Plan::Write {
        change: Change {
            file: file.clone(),
            before,
            after,
        },
        notes: vec![format!("the entry goes in {file}")],
        then: Then::Nothing,
    })
}

/// A create adds exactly its own key and nothing else.
fn refuse_structural_change_added(before: &str, after: &str, target: &str) -> Result<()> {
    let mut expected = keys_of(before);
    let mut got = keys_of(after);
    got.retain(|k| k != target);
    expected.retain(|k| k != target);
    if expected != got || !keys_of(after).iter().any(|k| k == target) {
        bail!(
            "the new text would change which other entries the file has (it carries a key heading)"
        );
    }
    Ok(())
}

// ---- rekey and delete ---------------------------------------------------

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn plan_rekey(root: &Path, target: &str, new_target: &str, force: bool, ack: bool) -> Result<Plan> {
    if target == new_target {
        bail!("rekey needs a `new_target` that differs from `target`");
    }
    if !new_target.contains("::") {
        bail!("`{new_target}` is not a symbol id (`file::qualified::name`)");
    }
    let Some(located) = locate(root, target)? else {
        if let Some((file, line)) = indexed_at(root, new_target)?.first() {
            return Ok(Plan::Done(format!(
                "`{new_target}` already has the entry, at {file}:{line}"
            )));
        }
        bail!("no glossary entry is keyed to `{target}`");
    };
    if let Some((file, line)) = indexed_at(root, new_target)?.first() {
        bail!("`{new_target}` already has an entry in {file}:{line}");
    }
    if !force && !symbol_is_live(root, new_target)? {
        bail!(
            "`{new_target}` is not in the graph: rekey to a key that resolves, or pass force to \
             rekey anyway"
        );
    }
    let mut lines = split_lines(&located.text);
    let heading = &lines[located.entry.start - 1];
    let hashes = "#".repeat(located.entry.level);
    let trailing = heading.len() - heading.trim_end().len();
    let line = format!(
        "{hashes} `{new_target}`{}",
        &heading[heading.len() - trailing..]
    );
    lines[located.entry.start - 1] = line;
    let after = join_lines(&lines, &located.text);
    let keys = [target.to_string()];
    let mentions = link_check::with_docs(root, |q| q.code_links_for(LinkKind::Mentions, &keys))?;
    let mut notes = Vec::new();
    if !mentions.is_empty() {
        notes.push(format!(
            "{} of `{target}` remain in prose and are not edited ({})",
            plural(mentions.len(), "prose mention", "prose mentions"),
            mentions
                .iter()
                .take(5)
                .map(|m| format!("{}:{}", m.doc_file, m.line_start))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(Plan::Write {
        change: Change {
            file: located.file,
            before: located.text,
            after,
        },
        notes,
        then: if ack {
            Then::Ack(new_target.to_string())
        } else {
            Then::Nothing
        },
    })
}

fn plan_delete(root: &Path, target: &str, force: bool) -> Result<Plan> {
    let Some(located) = locate(root, target)? else {
        return Ok(Plan::Done(format!("no entry is keyed to `{target}`")));
    };
    if !force && symbol_is_live(root, target)? {
        bail!(
            "`{target}` is still in the graph: delete an entry once its symbol is gone, or pass \
             force to delete it anyway"
        );
    }
    let mut lines = split_lines(&located.text);
    let (start, end) = (located.entry.start, located.entry.end);
    // One adjoining blank line goes with it: the one after, else the one
    // before, so no double blank is left and a last entry leaves no trailing
    // one.
    let (from, to) = if lines.get(end).is_some_and(|l| is_blank(l)) {
        (start - 1, end + 1)
    } else if start >= 2 && is_blank(&lines[start - 2]) {
        (start - 2, end)
    } else {
        (start - 1, end)
    };
    lines.drain(from..to);
    let after = join_lines(&lines, &located.text);
    Ok(Plan::Write {
        change: Change {
            file: located.file,
            before: located.text,
            after,
        },
        notes: Vec::new(),
        then: Then::DropAck(target.to_string()),
    })
}

// ---- carrying out ---------------------------------------------------------

fn write_file(root: &Path, change: &Change) -> Result<()> {
    let path = doc_path(root, &change.file)?;
    let now = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read `{}` to write it", change.file))?;
    if now != change.before {
        bail!(
            "{} changed since the plan was made; nothing was written, ask again",
            change.file
        );
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.infigraph-tmp"));
    std::fs::write(&tmp, &change.after)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(&path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// The rest of an applied operation: refresh the index (a review needs the
/// entry's new key indexed), then forget or record a review. Each failure is
/// reported, never silent, and none undoes the edit.
fn after_write(root: &Path, then: Then) -> String {
    let mut out = String::new();
    match ops::request_index_docs(root, false) {
        Ok(stats) => out.push_str(&format!(
            "document index refreshed ({} of {} files indexed)\n",
            stats.files_indexed, stats.files_scanned
        )),
        Err(e) => out.push_str(&format!(
            "the file was written, but the index could not be refreshed ({e:#}): run `infigraph \
             index-docs`\n"
        )),
    }
    match then {
        Then::Nothing => {}
        Then::DropAck(target) => {
            if let Err(e) = ops::request_ack_docs(root, &[], &[target]) {
                out.push_str(&format!("could not forget its review: {e:#}\n"));
            }
        }
        Then::Ack(target) => match ack::acknowledge(root, std::slice::from_ref(&target)) {
            Ok(report) if report.refused.is_empty() => {
                out.push_str(&format!(
                    "`{target}` recorded as reviewed against its symbol\n"
                ));
            }
            Ok(report) => {
                for (t, why) in report.refused {
                    out.push_str(&format!("review of `{t}` refused: {why}\n"));
                }
            }
            Err(e) => out.push_str(&format!("review of `{target}` failed: {e:#}\n")),
        },
    }
    out
}

fn finish_review(root: &Path, head: &str, review: AckPlan, apply: bool) -> Result<String> {
    let mut out = String::new();
    let AckPlan { acks, refused } = review;
    if acks.is_empty() && refused.is_empty() {
        return Ok(format!("{head}: nothing to review"));
    }
    if !acks.is_empty() {
        let list = acks
            .iter()
            .map(|a| {
                format!(
                    "  {} (code as of {} {})",
                    a.target,
                    &a.commit[..a.commit.len().min(8)],
                    civil_date(a.commit_time)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if apply {
            ops::request_ack_docs(root, &acks, &[])?;
            out.push_str(&format!(
                "{head}: recorded a review of {}\n{list}\n",
                plural(acks.len(), "entry", "entries")
            ));
        } else {
            out.push_str(&format!(
                "{head}: dry run, nothing written (pass apply=true to record a review of {})\n{list}\n",
                plural(acks.len(), "entry", "entries")
            ));
        }
    } else {
        out.push_str(&format!("{head}: nothing can be reviewed\n"));
    }
    for (target, why) in refused {
        out.push_str(&format!("  refused `{target}`: {why}\n"));
    }
    Ok(out)
}

// ---- showing a change -----------------------------------------------------

const DIFF_CONTEXT: usize = 2;
const DIFF_MAX_LINES: usize = 60;

/// The changed lines of `before` -> `after` with a little context, as a
/// unified-diff-like block. Every operation here changes one contiguous run
/// of lines (or `replace_all` several, shown from the first to the last).
fn diff(file: &str, before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (old_end, new_end) = (old.len() - suffix, new.len() - suffix);
    let from = prefix.saturating_sub(DIFF_CONTEXT);
    let to_old = (old_end + DIFF_CONTEXT).min(old.len());
    let mut lines: Vec<String> = Vec::new();
    lines.extend(old[from..prefix].iter().map(|l| format!(" {l}")));
    lines.extend(old[prefix..old_end].iter().map(|l| format!("-{l}")));
    lines.extend(new[prefix..new_end].iter().map(|l| format!("+{l}")));
    lines.extend(old[old_end..to_old].iter().map(|l| format!(" {l}")));
    let mut out = format!("@@ {file}:{} @@\n", from + 1);
    let total = lines.len();
    for line in lines.iter().take(DIFF_MAX_LINES) {
        out.push_str(line);
        out.push('\n');
    }
    if total > DIFF_MAX_LINES {
        out.push_str(&format!("... ({} more lines)\n", total - DIFF_MAX_LINES));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diff_shows_only_the_changed_lines_with_two_lines_of_context() {
        let before = "a\nb\nc\nd\ne\nf\ng\n";
        let after = "a\nb\nc\nD\ne\nf\ng\n";
        let shown = diff("f.md", before, after);
        assert_eq!(shown, "@@ f.md:2 @@\n b\n c\n-d\n+D\n e\n f\n");
    }

    #[test]
    fn a_long_diff_is_cut_and_says_how_much() {
        let before = String::new();
        let after: String = (0..100).map(|n| format!("line {n}\n")).collect();
        let shown = diff("f.md", &before, &after);
        assert!(shown.contains("... (40 more lines)"), "{shown}");
    }

    #[test]
    fn body_lines_drop_surrounding_blank_lines_and_trailing_spaces() {
        assert_eq!(body_lines("\n\nOne  \nTwo\n\n"), ["One", "Two"]);
        assert!(body_lines(" \n \n").is_empty());
    }

    #[test]
    fn line_endings_and_the_final_newline_survive_a_rewrite() {
        let crlf = "a\r\nb\r\n";
        assert_eq!(join_lines(&split_lines(crlf), crlf), crlf);
        let bare = "a\nb";
        assert_eq!(join_lines(&split_lines(bare), bare), bare);
    }
}
