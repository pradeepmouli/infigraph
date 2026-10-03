//! What the last run of each kind did, durably (#209 items 9 and 11).
//!
//! A run can go wrong without leaving a lasting state: an enrichment refuses
//! an empty indexer output, a COPY drops edges, call resolution fails, an
//! embedding update fails. #75 keeps those out of the degraded modes because
//! they are events, so they reached only `.infigraph/daemon.log` or
//! `scip-enrich.log`. This module keeps one small record per kind of run,
//! written by whoever ran the work, and `doctor` / `get_stats` read it back.
//! Nothing decides from it.
//!
//! One file per kind (`last-run.<kind>.json`), each replaced whole with
//! `write_atomic`, so two kinds finishing together cannot lose one another and
//! no lock is needed. Two runs of the *same* kind finishing together (a
//! daemon enrichment and a detached `scip-enrich`) leave the later rename.
//!
//! Each file holds `last` and `last_problem`. The watcher drains every few
//! seconds and most drains are clean, so `last` alone would hide a loss within
//! seconds. `last_problem` stays until a run that repairs it:
//! a clean run of a kind that redoes everything it covers (reindex, SCIP
//! enrichment, embeddings), or a landed full reindex for the incremental
//! `index` kind. A clean drain repairs nothing.
//!
//! The counts a run loses are noted deep in write paths that have no run in
//! hand, so a run is opened as a [`Run`] and the sites call [`note`]. The
//! registry is process-wide (a thread-local would miss rayon workers) and
//! checks the assumption that one run is active per process at a time: a
//! second run beginning while one is active is logged and flagged on both
//! records, whose counts may then include each other's.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The kinds of run that leave a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A drain or incremental index.
    Index,
    /// A full reindex.
    Reindex,
    /// A SCIP enrichment: indexers run and imported.
    Scip,
    /// An embeddings update.
    Embeddings,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Index, Kind::Reindex, Kind::Scip, Kind::Embeddings];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Index => "index",
            Kind::Reindex => "reindex",
            Kind::Scip => "scip",
            Kind::Embeddings => "embeddings",
        }
    }

    /// How the kind reads in a sentence.
    pub fn describe(self) -> &'static str {
        match self {
            Kind::Index => "incremental index",
            Kind::Reindex => "full reindex",
            Kind::Scip => "SCIP enrichment",
            Kind::Embeddings => "embeddings update",
        }
    }

    fn file_name(self) -> String {
        format!("last-run.{}.json", self.name())
    }

    /// Whether a clean run of this kind clears this kind's own problem: true
    /// where one run redoes everything the problem could be about.
    fn clean_run_repairs_itself(self) -> bool {
        self != Kind::Index
    }

    /// The other kinds a run of this kind, once it landed, repairs.
    fn repairs(self) -> &'static [Kind] {
        match self {
            Kind::Reindex => &[Kind::Index],
            _ => &[],
        }
    }
}

/// One kind of thing a run lost or refused, with how many and the first
/// reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loss {
    pub what: String,
    pub count: u64,
    pub first_reason: String,
}

/// What one run did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Unix epoch seconds.
    pub finished_at: u64,
    /// The process that ran it.
    pub pid: u32,
    /// False when the run failed or an indexer's output was refused.
    pub ok: bool,
    pub summary: String,
    #[serde(default)]
    pub losses: Vec<Loss>,
    /// Another run was active in this process while this one ran, so its
    /// `losses` may include that run's.
    #[serde(default)]
    pub overlapped: bool,
}

impl RunRecord {
    pub fn new(ok: bool, summary: impl Into<String>, tally: Tally) -> Self {
        Self {
            finished_at: now_secs(),
            pid: std::process::id(),
            ok,
            summary: summary.into(),
            losses: tally.losses,
            overlapped: tally.overlapped,
        }
    }

    pub fn has_problem(&self) -> bool {
        !self.ok || !self.losses.is_empty()
    }
}

/// A kind's file: its last run, and the last run that had a problem.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindRecord {
    pub last: Option<RunRecord>,
    pub last_problem: Option<RunRecord>,
}

/// What a run noted: its losses, merged by `what`, and whether it overlapped
/// another run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub losses: Vec<Loss>,
    pub overlapped: bool,
}

impl Tally {
    /// Add `count` of `what`; the first reason for a `what` is kept.
    pub fn note(&mut self, what: &str, count: u64, reason: &str) {
        match self.losses.iter_mut().find(|l| l.what == what) {
            Some(loss) => loss.count += count,
            None => self.losses.push(Loss {
                what: what.to_string(),
                count,
                first_reason: reason.to_string(),
            }),
        }
    }
}

struct Active {
    id: u64,
    kind: Kind,
    tally: Tally,
}

/// The runs active in a process. The process-wide one is behind [`begin`] and
/// [`note`]; tests make their own.
pub struct Registry {
    active: Mutex<Vec<Active>>,
    next_id: AtomicU64,
}

impl Registry {
    pub const fn new() -> Self {
        Self {
            active: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Open a run. Notes made until it ends or drops reach it.
    pub fn begin(&self, kind: Kind) -> Run<'_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut active = self.lock();
        let overlapping = !active.is_empty();
        if overlapping {
            for other in active.iter_mut() {
                other.tally.overlapped = true;
            }
            eprintln!(
                "[last-run] a {} run began while a {} run was active in this process; \
                 their loss counts may include each other's",
                kind.name(),
                active
                    .iter()
                    .map(|a| a.kind.name())
                    .collect::<Vec<_>>()
                    .join("/")
            );
        }
        active.push(Active {
            id,
            kind,
            tally: Tally {
                losses: Vec::new(),
                overlapped: overlapping,
            },
        });
        Run { registry: self, id }
    }

    /// Add to every active run; nothing active drops the note.
    pub fn note(&self, what: &str, count: u64, reason: &str) {
        for run in self.lock().iter_mut() {
            run.tally.note(what, count, reason);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Active>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn take(&self, id: u64) -> Option<Tally> {
        let mut active = self.lock();
        let at = active.iter().position(|a| a.id == id)?;
        Some(active.remove(at).tally)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// An open run. [`Run::end`] returns what it noted; dropping it without
/// ending (a panic) just closes it.
pub struct Run<'a> {
    registry: &'a Registry,
    id: u64,
}

impl Run<'_> {
    pub fn end(self) -> Tally {
        self.registry.take(self.id).unwrap_or_default()
    }
}

impl Drop for Run<'_> {
    fn drop(&mut self) {
        // `end` has already taken it; a run dropped by a panic is closed here.
        let _ = self.registry.take(self.id);
    }
}

static GLOBAL: Registry = Registry::new();

/// Open a run in this process.
pub fn begin(kind: Kind) -> Run<'static> {
    GLOBAL.begin(kind)
}

/// Note a loss on the run(s) active in this process: `count` of `what`, with
/// the reason for the first. A no-op when no run is active.
pub fn note(what: &str, count: u64, reason: &str) {
    GLOBAL.note(what, count, reason)
}

/// Record `run` as the latest of `kind`, under `infigraph_dir`.
///
/// Best-effort: it logs and returns on a failure, like `fault::record`. A
/// missing `infigraph_dir` records nothing -- the project is gone, and writing
/// would `create_dir_all` it back (#136).
pub fn record(infigraph_dir: &Path, kind: Kind, run: RunRecord) {
    if !infigraph_dir.is_dir() {
        eprintln!(
            "[last-run] not recording the {} run: {} is gone",
            kind.name(),
            infigraph_dir.display()
        );
        return;
    }
    let mut next = read(infigraph_dir, kind).unwrap_or_default();
    if run.has_problem() {
        next.last_problem = Some(run.clone());
    } else if kind.clean_run_repairs_itself() {
        next.last_problem = None;
    }
    let landed = run.ok;
    next.last = Some(run);
    write(infigraph_dir, kind, &next);
    if landed {
        for repaired in kind.repairs() {
            if let Some(mut other) = read(infigraph_dir, *repaired) {
                if other.last_problem.take().is_some() {
                    write(infigraph_dir, *repaired, &other);
                }
            }
        }
    }
}

fn write(infigraph_dir: &Path, kind: Kind, record: &KindRecord) {
    let path = infigraph_dir.join(kind.file_name());
    let written = serde_json::to_string_pretty(record)
        .map_err(anyhow::Error::from)
        .and_then(|json| crate::daemon_protocol::write_atomic(&path, &json));
    if let Err(e) = written {
        eprintln!(
            "[last-run] could not record the {} run at {} ({e:#})",
            kind.name(),
            path.display()
        );
    }
}

/// A kind's record, or `None` when there is none or it cannot be read.
pub fn read(infigraph_dir: &Path, kind: Kind) -> Option<KindRecord> {
    let text = std::fs::read_to_string(infigraph_dir.join(kind.file_name())).ok()?;
    serde_json::from_str(&text).ok()
}

/// The `last_problem` of every kind that has one.
pub fn problems(infigraph_dir: &Path) -> Vec<(Kind, RunRecord)> {
    Kind::ALL
        .into_iter()
        .filter_map(|kind| Some((kind, read(infigraph_dir, kind)?.last_problem?)))
        .collect()
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// One line for a problem, for `doctor` and `get_stats`: the kind, how long
/// ago, and each loss with its count and first reason.
pub fn problem_line(kind: Kind, run: &RunRecord, now_secs: u64) -> String {
    let secs = now_secs.saturating_sub(run.finished_at);
    let age = match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    };
    let mut line = format!(
        "the last {} ({age} ago) had a problem: {}",
        kind.describe(),
        run.summary
    );
    for loss in &run.losses {
        line.push_str(&format!(
            "; {}: {} -- {}",
            loss.what, loss.count, loss.first_reason
        ));
    }
    if run.overlapped {
        line.push_str(" (counts may include another run's)");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tally(losses: &[(&str, u64, &str)]) -> Tally {
        Tally {
            losses: losses
                .iter()
                .map(|(what, count, reason)| Loss {
                    what: what.to_string(),
                    count: *count,
                    first_reason: reason.to_string(),
                })
                .collect(),
            overlapped: false,
        }
    }

    fn clean() -> RunRecord {
        RunRecord::new(true, "ok", Tally::default())
    }

    fn lossy() -> RunRecord {
        RunRecord::new(
            true,
            "ok",
            tally(&[("edges dropped", 3, "missing endpoint")]),
        )
    }

    fn failed() -> RunRecord {
        RunRecord::new(false, "boom", Tally::default())
    }

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
        dir
    }

    fn ig(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join(".infigraph")
    }

    // ---- the registry ----

    #[test]
    fn notes_reach_the_active_run_and_merge_by_what() {
        let reg = Registry::new();
        let run = reg.begin(Kind::Index);
        reg.note("edges dropped", 2, "first");
        reg.note("edges dropped", 3, "second");
        reg.note("resolve failed", 1, "boom");
        let t = run.end();
        assert_eq!(
            t.losses,
            vec![
                Loss {
                    what: "edges dropped".into(),
                    count: 5,
                    first_reason: "first".into()
                },
                Loss {
                    what: "resolve failed".into(),
                    count: 1,
                    first_reason: "boom".into()
                },
            ]
        );
        assert!(!t.overlapped);
    }

    #[test]
    fn a_note_with_no_active_run_is_dropped() {
        let reg = Registry::new();
        reg.note("edges dropped", 2, "nobody is listening");
        let t = reg.begin(Kind::Index).end();
        assert!(t.losses.is_empty());
    }

    #[test]
    fn notes_from_other_threads_reach_the_run() {
        let reg = Registry::new();
        let run = reg.begin(Kind::Reindex);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| reg.note("edges dropped", 1, "worker"));
            }
        });
        assert_eq!(run.end().losses[0].count, 4);
    }

    #[test]
    fn overlapping_runs_are_flagged_on_both_and_neither_is_silently_merged() {
        let reg = Registry::new();
        let first = reg.begin(Kind::Index);
        reg.note("before", 1, "only the first was active");
        let second = reg.begin(Kind::Scip);
        reg.note("during", 1, "both were active");
        let t2 = second.end();
        let t1 = first.end();
        assert!(
            t1.overlapped && t2.overlapped,
            "both must say they overlapped"
        );
        let whats = |t: &Tally| t.losses.iter().map(|l| l.what.clone()).collect::<Vec<_>>();
        assert_eq!(whats(&t1), vec!["before", "during"]);
        assert_eq!(whats(&t2), vec!["during"]);
    }

    #[test]
    fn a_run_after_an_overlap_has_ended_is_not_flagged() {
        let reg = Registry::new();
        let a = reg.begin(Kind::Index);
        let b = reg.begin(Kind::Scip);
        drop((a, b));
        assert!(!reg.begin(Kind::Index).end().overlapped);
    }

    #[test]
    fn a_run_dropped_without_ending_stops_receiving_notes() {
        let reg = Registry::new();
        drop(reg.begin(Kind::Index));
        let fresh = reg.begin(Kind::Index);
        reg.note("edges dropped", 1, "x");
        let t = fresh.end();
        assert!(!t.overlapped, "the dropped run must not count as active");
        assert_eq!(t.losses[0].count, 1);
    }

    // ---- the files ----

    #[test]
    fn a_problem_run_becomes_last_and_last_problem() {
        let p = project();
        record(&ig(&p), Kind::Scip, failed());
        let r = read(&ig(&p), Kind::Scip).unwrap();
        assert_eq!(r.last, r.last_problem);
        assert!(r.last.unwrap().has_problem());
    }

    #[test]
    fn a_clean_drain_does_not_hide_an_earlier_loss() {
        let p = project();
        record(&ig(&p), Kind::Index, lossy());
        record(&ig(&p), Kind::Index, clean());
        let r = read(&ig(&p), Kind::Index).unwrap();
        assert!(!r.last.unwrap().has_problem(), "last is the clean drain");
        assert!(
            r.last_problem.unwrap().has_problem(),
            "the loss stays until something repairs it"
        );
    }

    #[test]
    fn a_landed_full_reindex_repairs_the_index_problem_and_its_own() {
        let p = project();
        record(&ig(&p), Kind::Index, lossy());
        record(&ig(&p), Kind::Reindex, lossy());
        record(&ig(&p), Kind::Reindex, clean());
        assert_eq!(read(&ig(&p), Kind::Reindex).unwrap().last_problem, None);
        assert_eq!(read(&ig(&p), Kind::Index).unwrap().last_problem, None);
    }

    #[test]
    fn a_reindex_that_lost_edges_still_repairs_the_drains_but_keeps_its_own_loss() {
        let p = project();
        record(&ig(&p), Kind::Index, lossy());
        record(&ig(&p), Kind::Reindex, lossy());
        assert_eq!(read(&ig(&p), Kind::Index).unwrap().last_problem, None);
        assert!(read(&ig(&p), Kind::Reindex).unwrap().last_problem.is_some());
    }

    #[test]
    fn a_failed_reindex_repairs_nothing() {
        let p = project();
        record(&ig(&p), Kind::Index, lossy());
        record(&ig(&p), Kind::Reindex, failed());
        assert!(read(&ig(&p), Kind::Index).unwrap().last_problem.is_some());
    }

    #[test]
    fn a_clean_scip_or_embeddings_run_clears_its_own_problem() {
        let p = project();
        for kind in [Kind::Scip, Kind::Embeddings] {
            record(&ig(&p), kind, failed());
            record(&ig(&p), kind, clean());
            assert_eq!(read(&ig(&p), kind).unwrap().last_problem, None, "{kind:?}");
        }
    }

    #[test]
    fn kinds_are_separate_files() {
        let p = project();
        record(&ig(&p), Kind::Scip, failed());
        assert!(ig(&p).join("last-run.scip.json").exists());
        assert!(!ig(&p).join("last-run.embeddings.json").exists());
        assert_eq!(read(&ig(&p), Kind::Embeddings), None);
    }

    #[test]
    fn a_missing_infigraph_dir_is_not_recreated() {
        let dir = tempfile::tempdir().unwrap();
        record(&dir.path().join(".infigraph"), Kind::Index, lossy());
        assert!(!dir.path().join(".infigraph").exists());
    }

    #[test]
    fn a_corrupt_file_reads_as_none_and_the_next_record_replaces_it() {
        let p = project();
        std::fs::write(ig(&p).join("last-run.index.json"), b"{not json").unwrap();
        assert_eq!(read(&ig(&p), Kind::Index), None);
        record(&ig(&p), Kind::Index, clean());
        assert!(read(&ig(&p), Kind::Index).unwrap().last.is_some());
    }

    #[test]
    fn problems_lists_only_kinds_with_one() {
        let p = project();
        record(&ig(&p), Kind::Index, lossy());
        record(&ig(&p), Kind::Scip, clean());
        record(&ig(&p), Kind::Embeddings, failed());
        let kinds: Vec<Kind> = problems(&ig(&p)).into_iter().map(|(k, _)| k).collect();
        assert_eq!(kinds, vec![Kind::Index, Kind::Embeddings]);
    }

    #[test]
    fn a_record_carries_the_overlap_flag_of_its_tally() {
        let mut t = tally(&[("edges dropped", 1, "x")]);
        t.overlapped = true;
        assert!(RunRecord::new(true, "ok", t).overlapped);
    }

    #[test]
    fn a_problem_line_names_the_kind_the_age_and_each_loss() {
        let mut r = RunRecord::new(
            false,
            "scip-python output rejected",
            tally(&[("edges dropped", 3, "missing endpoint")]),
        );
        r.finished_at = 1_000;
        let line = problem_line(Kind::Scip, &r, 1_000 + 12 * 60);
        for part in [
            "SCIP enrichment",
            "12m ago",
            "scip-python output rejected",
            "edges dropped",
            "3",
            "missing endpoint",
        ] {
            assert!(line.contains(part), "{part:?} missing from {line:?}");
        }
    }
}
