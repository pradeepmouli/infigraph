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
//! registry is process-wide (a thread-local would miss rayon workers). A
//! note made on a thread that opened a run belongs to that run -- the
//! innermost, if it opened several -- so concurrent runs on different threads
//! (another project's, or a different kind) keep their losses apart. A note
//! from a thread that opened none cannot be placed and reaches every active
//! run. A second run beginning while one is active is logged and flagged on
//! both records either way, since the unplaceable notes may then be
//! counted in each.

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

    /// What to do about a problem of this kind.
    pub fn remedy(self) -> &'static str {
        match self {
            Kind::Index => {
                "run `infigraph index --full` to rebuild the graph; the cause is in \
                 .infigraph/daemon.log"
            }
            Kind::Reindex => {
                "fix the cause named in .infigraph/daemon.log, then run \
                 `infigraph index --full` again"
            }
            Kind::Scip => {
                "run `infigraph index` to try again; the indexer's own output is in \
                 .infigraph/daemon.log or .infigraph/scip-enrich.log"
            }
            Kind::Embeddings => {
                "run `infigraph index` to try again; the cause is in .infigraph/daemon.log"
            }
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

/// One open run. **A run belongs to the thread that opened it, so it must be
/// begun and ended on one thread and never held across an `.await`**: a task
/// that resumes on another worker would note into a thread that owns no run
/// (the note then reaches every run) or, worse, into a run another task began
/// on that worker. [`Run`] is `!Send` so the compiler refuses both. Every
/// owner today is a plain function or a `spawn_blocking` closure, which run
/// start to finish on one thread.
struct Active {
    id: u64,
    kind: Kind,
    /// The thread that opened the run: a note made there is its own.
    thread: std::thread::ThreadId,
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
            thread: std::thread::current().id(),
            tally: Tally {
                losses: Vec::new(),
                overlapped: overlapping,
            },
        });
        Run {
            registry: self,
            id,
            _not_send: std::marker::PhantomData,
        }
    }

    /// Add a loss to the run it belongs to. A thread that opened a run notes
    /// into the one it opened last (an update nested inside another run lost
    /// its own things, not the outer run's), so two runs on two threads never
    /// take each other's losses. A thread that opened none -- a rayon worker
    /// -- cannot say which run it works for, so its note goes to every active
    /// run, which `begin` has already flagged as overlapping. Nothing active
    /// drops the note.
    pub fn note(&self, what: &str, count: u64, reason: &str) {
        let me = std::thread::current().id();
        let mut active = self.lock();
        match active.iter_mut().rev().find(|run| run.thread == me) {
            Some(own) => own.tally.note(what, count, reason),
            None => {
                for run in active.iter_mut() {
                    run.tally.note(what, count, reason);
                }
            }
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
    /// `!Send`: the run is owned by the thread that opened it (see `Active`).
    _not_send: std::marker::PhantomData<*const ()>,
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

#[cfg(not(test))]
static GLOBAL: Registry = Registry::new();

/// The registry the free functions use. One per process, because the sites
/// that note a loss may run on any thread (a rayon worker), not the one that
/// opened the run.
#[cfg(not(test))]
fn global() -> &'static Registry {
    &GLOBAL
}

/// Under `cargo test` every test is a thread of one process, and a test that
/// drops edges on purpose would note into another test's run. So a lib test
/// sees only its own thread's registry; `Registry` itself is tested directly,
/// including notes from several threads.
#[cfg(test)]
fn global() -> &'static Registry {
    thread_local! {
        static PER_THREAD: &'static Registry = Box::leak(Box::new(Registry::new()));
    }
    PER_THREAD.with(|registry| *registry)
}

/// Open a run in this process.
pub fn begin(kind: Kind) -> Run<'static> {
    global().begin(kind)
}

/// Note a loss on the run(s) active in this process: `count` of `what`, with
/// the reason for the first. A no-op when no run is active.
pub fn note(what: &str, count: u64, reason: &str) {
    global().note(what, count, reason)
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
    let skip = is_repeat(&next, kind, &run);
    if run.has_problem() {
        next.last_problem = Some(run.clone());
    } else if kind.clean_run_repairs_itself() {
        next.last_problem = None;
    }
    let landed = run.ok;
    next.last = Some(run);
    if !skip {
        write(infigraph_dir, kind, &next);
    }
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

/// How long a clean run after a clean one leaves the file alone.
const QUIET_SECS: u64 = 60;

/// Whether `run` adds nothing to what `prev` already says, so the file is not
/// rewritten (an fsync per drain, every few seconds, for nothing): a clean run
/// within [`QUIET_SECS`] of a clean one that repairs no problem, or a failure
/// worded exactly like the last (a daemon retrying a failing drain every
/// tick). The record keeps the first time.
fn is_repeat(prev: &KindRecord, kind: Kind, run: &RunRecord) -> bool {
    let Some(last) = &prev.last else {
        return false;
    };
    if !run.has_problem() {
        let repairs = kind.clean_run_repairs_itself() && prev.last_problem.is_some();
        return !repairs
            && !last.has_problem()
            && run.finished_at.saturating_sub(last.finished_at) < QUIET_SECS;
    }
    !run.ok
        && run.losses.is_empty()
        && !last.ok
        && last.losses.is_empty()
        && last.summary == run.summary
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

/// The section `get_stats` shows: one line per kind with an unrepaired
/// problem, or nothing at all when there is none (unlike the degraded modes'
/// "none", a clean project has nothing to say here).
pub fn render_problems(infigraph_dir: &Path, now_secs: u64) -> String {
    let problems = problems(infigraph_dir);
    if problems.is_empty() {
        return String::new();
    }
    let mut out = String::from("Last runs with problems:");
    for (kind, run) in problems {
        out.push_str("\n  ");
        out.push_str(&problem_line(kind, &run, now_secs));
    }
    out
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
    fn overlapping_runs_are_flagged_on_both_and_a_note_goes_to_the_innermost() {
        // Two runs opened on one thread: a note made there belongs to the one
        // begun last (an embeddings update inside a reindex lost embeddings,
        // not edges), and both records say they overlapped.
        let reg = Registry::new();
        let first = reg.begin(Kind::Index);
        reg.note("before", 1, "only the first was active");
        let second = reg.begin(Kind::Scip);
        reg.note("during", 1, "the innermost run was active");
        let t2 = second.end();
        let t1 = first.end();
        assert!(
            t1.overlapped && t2.overlapped,
            "both must say they overlapped"
        );
        let whats = |t: &Tally| t.losses.iter().map(|l| l.what.clone()).collect::<Vec<_>>();
        assert_eq!(whats(&t1), vec!["before"]);
        assert_eq!(whats(&t2), vec!["during"]);
    }

    /// The rule behind thread ownership, enforced by the compiler: a `Run`
    /// belongs to the thread that opened it, so it cannot be sent to another
    /// thread or held across an `.await` in a task that may resume on another
    /// worker (a future holding it is not `Send`). This fails to compile --
    /// "type annotations needed", the ambiguity of two matching impls -- if
    /// `Run` ever becomes `Send`.
    #[allow(dead_code)]
    fn run_is_not_send() {
        struct IsSend;
        trait AmbiguousIfSend<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<IsSend> for T {}
        let _ = <Run<'static> as AmbiguousIfSend<_>>::check;
    }

    #[test]
    fn a_note_stays_with_the_run_its_own_thread_opened() {
        // Two runs on two threads at once (two projects in one process, or an
        // embeddings update beside a drain): each thread's losses are its own
        // run's, however the two interleave.
        use std::sync::mpsc::channel;
        let reg = &Registry::new();
        let (opened_tx, opened_rx) = channel();
        let (go_tx, go_rx) = channel::<()>();
        let (noted_tx, noted_rx) = channel();
        let (mine, theirs) = std::thread::scope(|s| {
            let other = s.spawn(move || {
                let run = reg.begin(Kind::Embeddings);
                opened_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                reg.note("symbols not embedded", 1, "theirs");
                noted_tx.send(()).unwrap();
                run.end()
            });
            opened_rx.recv().unwrap();
            let mine = reg.begin(Kind::Embeddings);
            go_tx.send(()).unwrap();
            noted_rx.recv().unwrap();
            (mine.end(), other.join().unwrap())
        });
        assert!(
            mine.losses.is_empty(),
            "a loss another thread's run noted reached this one: {mine:?}"
        );
        assert_eq!(theirs.losses[0].count, 1);
        assert!(
            mine.overlapped && theirs.overlapped,
            "they still ran at the same time"
        );
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

    fn at(mut run: RunRecord, finished_at: u64) -> RunRecord {
        run.finished_at = finished_at;
        run
    }

    fn failed_with(summary: &str) -> RunRecord {
        RunRecord::new(false, summary, Tally::default())
    }

    fn last_finished(p: &tempfile::TempDir, kind: Kind) -> u64 {
        read(&ig(p), kind).unwrap().last.unwrap().finished_at
    }

    #[test]
    fn a_clean_run_soon_after_a_clean_one_is_not_rewritten() {
        let p = project();
        record(&ig(&p), Kind::Index, at(clean(), 1_000));
        record(&ig(&p), Kind::Index, at(clean(), 1_010));
        assert_eq!(last_finished(&p, Kind::Index), 1_000);
    }

    #[test]
    fn a_clean_run_a_minute_after_a_clean_one_is_written() {
        let p = project();
        record(&ig(&p), Kind::Index, at(clean(), 1_000));
        record(&ig(&p), Kind::Index, at(clean(), 1_061));
        assert_eq!(last_finished(&p, Kind::Index), 1_061);
    }

    #[test]
    fn a_clean_run_that_repairs_a_problem_is_written_at_once() {
        let p = project();
        record(&ig(&p), Kind::Scip, at(failed(), 1_000));
        record(&ig(&p), Kind::Scip, at(clean(), 1_001));
        assert_eq!(read(&ig(&p), Kind::Scip).unwrap().last_problem, None);
        assert_eq!(last_finished(&p, Kind::Scip), 1_001);
    }

    #[test]
    fn a_problem_after_a_clean_run_is_written_at_once() {
        let p = project();
        record(&ig(&p), Kind::Index, at(clean(), 1_000));
        record(&ig(&p), Kind::Index, at(lossy(), 1_001));
        assert!(read(&ig(&p), Kind::Index).unwrap().last_problem.is_some());
        assert_eq!(last_finished(&p, Kind::Index), 1_001);
    }

    #[test]
    fn a_run_that_fails_the_same_way_as_the_last_is_not_rewritten() {
        // A daemon retrying a failing drain every tick would otherwise
        // rewrite the file every tick; the record keeps the first time.
        let p = project();
        record(&ig(&p), Kind::Index, at(failed_with("boom"), 1_000));
        record(&ig(&p), Kind::Index, at(failed_with("boom"), 1_010));
        assert_eq!(last_finished(&p, Kind::Index), 1_000);
        record(&ig(&p), Kind::Index, at(failed_with("other"), 1_020));
        assert_eq!(last_finished(&p, Kind::Index), 1_020);
    }

    #[test]
    fn a_landed_reindex_still_repairs_when_its_own_file_is_not_rewritten() {
        let p = project();
        record(&ig(&p), Kind::Reindex, at(clean(), 1_000));
        record(&ig(&p), Kind::Index, at(lossy(), 1_001));
        record(&ig(&p), Kind::Reindex, at(clean(), 1_005));
        assert_eq!(read(&ig(&p), Kind::Index).unwrap().last_problem, None);
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
    fn the_stats_section_is_empty_when_nothing_is_wrong() {
        let p = project();
        assert_eq!(render_problems(&ig(&p), 5_000), "");
        record(&ig(&p), Kind::Index, clean());
        assert_eq!(render_problems(&ig(&p), 5_000), "");
    }

    #[test]
    fn the_stats_section_lists_each_kind_with_a_problem() {
        let p = project();
        record(&ig(&p), Kind::Index, at(lossy(), 1_000));
        record(&ig(&p), Kind::Embeddings, at(failed(), 1_000));
        let section = render_problems(&ig(&p), 1_000 + 2 * 3600);
        assert!(section.starts_with("Last runs with problems:"), "{section}");
        assert!(section.contains("incremental index"), "{section}");
        assert!(section.contains("embeddings update"), "{section}");
        assert!(section.contains("2h ago"), "{section}");
        assert_eq!(section.lines().count(), 3, "{section}");
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
