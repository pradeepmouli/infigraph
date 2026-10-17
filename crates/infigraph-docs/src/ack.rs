//! Entries reviewed against their code.
//!
//! "Possibly stale" says the symbol's lines changed after the entry did. Most
//! such changes leave the entry true, and a reviewer needs a way to say so
//! that does not mean editing prose. An acknowledgement is that: the entry
//! for a target was read against the symbol as of a commit (`DocAck`, in
//! `docs.kuzu`, see `ack_store`). [`verdict`](crate::stale_docs::verdict)
//! treats the entry as current for code up to that commit's time, so a later
//! change to the symbol makes it stale again. Nothing is written to the
//! markdown, and infigraph never decides an entry is true on its own: the
//! reviewer does, by calling [`acknowledge`].

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use infigraph_core::daemon_protocol::DocAck;
use infigraph_core::docs_switch::{self, DocsNotIndexed};
use infigraph_core::git_blame::{self, RangeBlame, GIT_TIMEOUT};

use crate::backend::DocBackend;
use crate::code_links::LinkKind;
use crate::link_check;

/// The reviews held for some targets.
#[derive(Debug, Default)]
pub struct Acks(HashMap<String, DocAck>);

impl Acks {
    /// No reviews: every entry is judged by its history alone.
    pub fn none() -> Self {
        Self::default()
    }

    /// Reviews built in memory, for tests of the rule that reads them.
    #[cfg(test)]
    pub(crate) fn from_rows(rows: impl IntoIterator<Item = DocAck>) -> Self {
        Self(rows.into_iter().map(|a| (a.target.clone(), a)).collect())
    }

    /// The reviews of `targets` held by `store`.
    pub fn of(store: &dyn DocBackend, targets: &[String]) -> Result<Self> {
        Ok(Self(store.doc_acks(targets)?))
    }

    /// Whether `target` was reviewed against code at least as new as `code`.
    /// Uncommitted lines are newer than any commit, so nothing covers them.
    pub fn covers(&self, target: &str, code: &RangeBlame) -> bool {
        !code.dirty
            && self
                .0
                .get(target)
                .is_some_and(|ack| ack.commit_time >= code.time)
    }
}

/// The reviews of `targets` in the project's document store, read through the
/// daemon when one is live (the way every other document read goes).
pub(crate) fn load(root: &Path, targets: &[String]) -> Result<Acks> {
    link_check::with_docs(root, |q| q.doc_acks(targets)).map(Acks)
}

/// What [`acknowledge`] did, per target.
#[derive(Debug, Default)]
pub struct AckReport {
    /// Targets whose review was recorded.
    pub acked: Vec<String>,
    /// Targets that could not be reviewed, with why.
    pub refused: Vec<(String, String)>,
}

/// What [`plan`] would record: the reviews and the targets that cannot be.
#[derive(Debug, Default)]
pub struct AckPlan {
    pub acks: Vec<DocAck>,
    pub refused: Vec<(String, String)>,
}

/// Records that the entries keyed to `targets` were reviewed against their
/// symbols as they are now (see [`plan`]). The write goes to the store through
/// the daemon like any docs write. Errors only when the project has no
/// document index or the write fails.
pub fn acknowledge(root: &Path, targets: &[String]) -> Result<AckReport> {
    let AckPlan { acks, refused } = plan(root, targets)?;
    if !acks.is_empty() {
        crate::ops::request_ack_docs(root, &acks, &[])?;
    }
    Ok(AckReport {
        acked: acks.into_iter().map(|a| a.target).collect(),
        refused,
    })
}

/// What reviewing `targets` now would record: the commit that last changed
/// each symbol's lines, by `git blame`. A target with no entry, with no
/// symbol, whose lines have uncommitted edits (a review names a commit, and
/// the next commit would make it stale again at once), or that git cannot
/// answer for, is refused with the reason. Reads only.
pub fn plan(root: &Path, targets: &[String]) -> Result<AckPlan> {
    if !docs_switch::docs_enabled(root) || !docs_switch::docs_store_path(root).exists() {
        return Err(DocsNotIndexed.into());
    }
    let mut report = AckPlan::default();
    let held = link_check::with_docs(root, |q| q.code_links_for(LinkKind::Documents, targets))?;
    let (with_entry, without): (Vec<&String>, Vec<&String>) = targets
        .iter()
        .partition(|t| held.iter().any(|row| &row.target == *t));
    for target in without {
        report.refused.push((
            target.clone(),
            format!("no glossary entry is keyed to `{target}`"),
        ));
    }

    let keys: Vec<&str> = with_entry.iter().map(|t| t.as_str()).collect();
    let live = link_check::live_symbols(root, &keys)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    for target in with_entry {
        let Some(symbol) = live.get(target) else {
            report.refused.push((
                target.clone(),
                format!(
                    "the symbol `{target}` is no longer in the graph: rekey the entry to where it \
                     moved, or delete it"
                ),
            ));
            continue;
        };
        let lines = (symbol.start, symbol.end);
        let blame = match git_blame::blame_ranges_cached(root, &symbol.file, &[lines], GIT_TIMEOUT)
        {
            Ok(blames) => blames.into_iter().next(),
            Err(e) => {
                report
                    .refused
                    .push((target.clone(), format!("git could not answer: {e}")));
                continue;
            }
        };
        match blame {
            Some(code) if code.dirty => report.refused.push((
                target.clone(),
                format!(
                    "{}:{}-{} has uncommitted edits: commit them first, a review names a commit",
                    symbol.file, lines.0, lines.1
                ),
            )),
            Some(code) => {
                report.acks.push(DocAck {
                    target: target.clone(),
                    commit: code.commit,
                    commit_time: code.time,
                    acked_at: now,
                });
            }
            None => report.refused.push((
                target.clone(),
                format!(
                    "git gave no answer for {}:{}-{}",
                    symbol.file, lines.0, lines.1
                ),
            )),
        }
    }
    Ok(report)
}
