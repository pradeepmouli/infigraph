//! When the lines of a file last changed, from `git blame`.
//!
//! Clone-stable on purpose: a file's mtime is reset by a checkout and an
//! index time by a rebuild, but history is the same in every clone. Nothing
//! here is stored; a caller asks when it needs the answer.
//!
//! - [`blame_ranges`] is a pure function of (repo root, file, line ranges):
//!   per range, the newest commit that touched any of its lines. It runs git
//!   under a timeout and caches nothing.
//! - [`blame_ranges_cached`] is the thin layer a long-lived process wants: the
//!   same answer remembered per (file, HEAD, content of the file as it is on
//!   disk now), so an edit or a new commit is a new key.
//! - [`compare`] decides whether one range's content is older than another's,
//!   and [`SyncState::label`] words the answer. One definition for `doctor` and
//!   `get_doc_context`.
//!
//! Uncommitted lines count as the newest change, and `-w` keeps a whitespace
//! reflow from reading as an edit. A moved block reads as changed by the move
//! (`-M` was measured to rescue 2 of 821 flagged entries at twice the cost,
//! so it is not used).

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one git call may run before it is abandoned.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(3);

/// Why a blame could not be had. Either way the caller says "unknown", never
/// guesses, and never waits longer than the timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlameError {
    TimedOut,
    Failed(String),
}

impl std::fmt::Display for BlameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlameError::TimedOut => write!(f, "git did not answer in time"),
            BlameError::Failed(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for BlameError {}

/// The newest change within a range of lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeBlame {
    pub commit: String,
    /// Committer time, seconds since the epoch.
    pub time: i64,
    /// The newest change in the range is not committed yet.
    pub dirty: bool,
}

impl RangeBlame {
    pub fn short_commit(&self) -> &str {
        if self.dirty {
            "uncommitted"
        } else {
            &self.commit[..self.commit.len().min(8)]
        }
    }
}

/// Runs `program args` in `dir`, killing it after `timeout`. Output is drained
/// on threads, so a large answer cannot fill a pipe and stall the child.
fn run(
    program: &str,
    args: &[String],
    dir: &Path,
    timeout: Duration,
) -> Result<String, BlameError> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| BlameError::Failed(format!("cannot run {program}: {e}")))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let mut bytes = Vec::new();
                let _ = pipe.read_to_end(&mut bytes);
                text = String::from_utf8_lossy(&bytes).into_owned();
            }
            text
        })
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BlameError::TimedOut);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(BlameError::Failed(format!("waiting for {program}: {e}"))),
        }
    };
    let (out, err) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    if status.success() {
        Ok(out)
    } else {
        Err(BlameError::Failed(
            err.lines()
                .next()
                .unwrap_or("git failed")
                .trim()
                .to_string(),
        ))
    }
}

/// One blamed line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    number: usize,
    commit: String,
    time: i64,
}

/// The lines of `git blame --porcelain` output. A commit's header (with its
/// `committer-time`) appears only before its first line.
fn parse_porcelain(output: &str) -> Vec<Line> {
    let mut times: HashMap<&str, i64> = HashMap::new();
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut current: Option<&str> = None;
    for row in output.lines() {
        if row.starts_with('\t') {
            continue;
        }
        let mut fields = row.split(' ');
        let first = fields.next().unwrap_or("");
        if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            // `<sha> <original line> <final line> [<group size>]`
            if let Some(number) = fields.nth(1).and_then(|n| n.parse().ok()) {
                current = Some(first);
                lines.push((number, first));
            }
        } else if first == "committer-time" {
            if let (Some(commit), Some(time)) =
                (current, fields.next().and_then(|t| t.parse().ok()))
            {
                times.entry(commit).or_insert(time);
            }
        }
    }
    lines
        .into_iter()
        .map(|(number, commit)| Line {
            number,
            commit: commit.to_string(),
            time: times.get(commit).copied().unwrap_or(0),
        })
        .collect()
}

fn is_uncommitted(commit: &str) -> bool {
    commit.bytes().all(|b| b == b'0')
}

/// The newest change among `lines` numbered within `range`.
fn newest_in(lines: &[Line], range: (usize, usize)) -> Option<RangeBlame> {
    let mut best: Option<RangeBlame> = None;
    for line in lines
        .iter()
        .filter(|l| (range.0..=range.1).contains(&l.number))
    {
        let dirty = is_uncommitted(&line.commit);
        let candidate = RangeBlame {
            commit: line.commit.clone(),
            time: line.time,
            dirty,
        };
        best = match best {
            None => Some(candidate),
            Some(b) if b.dirty => Some(b),
            Some(_) if dirty => Some(candidate),
            Some(b) if candidate.time > b.time => Some(candidate),
            keep => keep,
        };
    }
    best
}

/// `ranges` (1-based, inclusive) without overlaps, in order: git blame
/// reports a line once however many `-L` options name it.
fn merged(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut sorted: Vec<(usize, usize)> = ranges.to_vec();
    sorted.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (start, end) in sorted {
        match out.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// Per range of `ranges` (1-based, inclusive lines of `file`, a path relative
/// to `root`), the newest change to any of its lines. One git call for all of
/// them. A range with no blamed line (past the end of the file) is an error:
/// the answer would be about some other text.
pub fn blame_ranges(
    root: &Path,
    file: &str,
    ranges: &[(usize, usize)],
    timeout: Duration,
) -> Result<Vec<RangeBlame>, BlameError> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    let mut args: Vec<String> = ["--no-pager", "blame", "--porcelain", "-w"]
        .map(String::from)
        .to_vec();
    for (start, end) in merged(ranges) {
        args.push("-L".to_string());
        args.push(format!("{start},{end}"));
    }
    args.extend(["--".to_string(), file.to_string()]);
    let lines = parse_porcelain(&run("git", &args, root, timeout)?);
    ranges
        .iter()
        .map(|range| {
            newest_in(&lines, *range).ok_or_else(|| {
                BlameError::Failed(format!(
                    "{file}:{}-{} is not in the file as it is now",
                    range.0, range.1
                ))
            })
        })
        .collect()
}

type CacheKey = (String, String, String, u64, Vec<(usize, usize)>);

/// Enough for a long session without growing without bound.
const CACHE_ENTRIES: usize = 2_048;

fn cache() -> &'static Mutex<HashMap<CacheKey, Vec<RangeBlame>>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<CacheKey, Vec<RangeBlame>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// [`blame_ranges`], remembered for this process per (file, HEAD, content of
/// the file now). A new commit or an edit changes the key, so nothing here can
/// go stale; only a failure is never remembered.
pub fn blame_ranges_cached(
    root: &Path,
    file: &str,
    ranges: &[(usize, usize)],
    timeout: Duration,
) -> Result<Vec<RangeBlame>, BlameError> {
    let head = run(
        "git",
        &["rev-parse".to_string(), "HEAD".to_string()],
        root,
        timeout,
    )?
    .trim()
    .to_string();
    let content = std::fs::read(root.join(file))
        .map_err(|e| BlameError::Failed(format!("cannot read {file}: {e}")))?;
    let key: CacheKey = (
        root.to_string_lossy().into_owned(),
        file.to_string(),
        head,
        crate::embed::fnv1a64(&content),
        ranges.to_vec(),
    );
    if let Some(hit) = cache().lock().ok().and_then(|c| c.get(&key).cloned()) {
        return Ok(hit);
    }
    let fresh = blame_ranges(root, file, ranges, timeout)?;
    if let Ok(mut c) = cache().lock() {
        if c.len() >= CACHE_ENTRIES {
            c.clear();
        }
        c.insert(key, fresh.clone());
    }
    Ok(fresh)
}

/// The commit time of the last commit that touched `file` at all, or `None`
/// when it has none. The coarse variant: it cannot tell which lines.
pub fn last_commit_of_file(
    root: &Path,
    file: &str,
    timeout: Duration,
) -> Result<Option<RangeBlame>, BlameError> {
    let args: Vec<String> = ["--no-pager", "log", "-1", "--format=%H %ct", "--", file]
        .map(String::from)
        .to_vec();
    let out = run("git", &args, root, timeout)?;
    Ok(out.split_once(' ').and_then(|(commit, time)| {
        Some(RangeBlame {
            commit: commit.to_string(),
            time: time.trim().parse().ok()?,
            dirty: false,
        })
    }))
}

/// Whether the content of a range is older than what it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    InSync,
    PossiblyStale { code: RangeBlame, entry: RangeBlame },
}

/// `code` is the newest change to the code, `entry` the newest to the text
/// that describes it. The same commit is in sync (its time is compared only
/// when the commits differ); an uncommitted change is the newest; two
/// uncommitted changes cannot be ordered and are taken as in sync.
pub fn compare(code: &RangeBlame, entry: &RangeBlame) -> SyncState {
    let stale = if code.commit == entry.commit {
        false
    } else if code.dirty {
        true
    } else if entry.dirty {
        false
    } else {
        code.time > entry.time
    };
    if stale {
        SyncState::PossiblyStale {
            code: code.clone(),
            entry: entry.clone(),
        }
    } else {
        SyncState::InSync
    }
}

/// `YYYY-MM-DD` (UTC) of a time in seconds since the epoch.
pub fn civil_date(seconds: i64) -> String {
    // Howard Hinnant's days-to-civil.
    let days = seconds.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

impl RangeBlame {
    /// `2026-10-11 49fb64ee`, or `uncommitted`.
    pub fn describe(&self) -> String {
        if self.dirty {
            "uncommitted".to_string()
        } else {
            format!("{} {}", civil_date(self.time), self.short_commit())
        }
    }
}

impl SyncState {
    /// The label every renderer shows for a stale pair; `None` when in sync.
    pub fn label(&self) -> Option<String> {
        match self {
            SyncState::InSync => None,
            SyncState::PossiblyStale { code, entry } => Some(format!(
                "possibly stale: code changed {}, entry last touched {}",
                code.describe(),
                entry.describe()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ZERO: &str = "0000000000000000000000000000000000000000";

    fn porcelain() -> String {
        format!(
            "{A} 1 1 2\nauthor x\ncommitter-time 1000\nsummary one\nfilename f\n\tline 1\n\
             {A} 2 2\n\tline 2\n\
             {B} 3 3 1\nauthor y\ncommitter-time 2000\nsummary two\nfilename f\n\tline 3\n\
             {ZERO} 4 4 1\nauthor Not Committed Yet\ncommitter-time 3000\nsummary s\nfilename f\n\tline 4\n"
        )
    }

    #[test]
    fn the_porcelain_gives_each_line_its_commit_and_the_time_of_its_first_header() {
        let lines = parse_porcelain(&porcelain());
        let numbers: Vec<_> = lines.iter().map(|l| (l.number, l.time)).collect();
        assert_eq!(numbers, [(1, 1000), (2, 1000), (3, 2000), (4, 3000)]);
        assert_eq!(lines[1].commit, A, "a later line of a commit has no header");
    }

    #[test]
    fn a_range_is_its_newest_change_and_an_uncommitted_line_is_the_newest() {
        let lines = parse_porcelain(&porcelain());
        let clean = newest_in(&lines, (1, 3)).unwrap();
        assert_eq!(
            (clean.commit.as_str(), clean.time, clean.dirty),
            (B, 2000, false)
        );
        let dirty = newest_in(&lines, (2, 4)).unwrap();
        assert!(dirty.dirty && dirty.describe() == "uncommitted");
        assert_eq!(newest_in(&lines, (9, 9)), None);
    }

    #[test]
    fn overlapping_and_touching_ranges_are_merged() {
        assert_eq!(
            merged(&[(5, 9), (1, 3), (4, 4), (8, 12), (20, 21)]),
            [(1, 12), (20, 21)]
        );
    }

    fn at(commit: &str, time: i64, dirty: bool) -> RangeBlame {
        RangeBlame {
            commit: commit.to_string(),
            time,
            dirty,
        }
    }

    #[test]
    fn the_same_commit_is_in_sync_whatever_the_times() {
        assert_eq!(
            compare(&at(A, 9, false), &at(A, 1, false)),
            SyncState::InSync
        );
    }

    #[test]
    fn code_changed_after_the_entry_is_possibly_stale_and_the_label_says_both_dates() {
        let stale = compare(&at(B, 1_760_140_800, false), &at(A, 1_758_758_400, false));
        assert_eq!(
            stale.label().as_deref(),
            Some("possibly stale: code changed 2025-10-11 bbbbbbbb, entry last touched 2025-09-25 aaaaaaaa")
        );
        assert_eq!(
            compare(&at(A, 1, false), &at(B, 2, false)),
            SyncState::InSync
        );
    }

    #[test]
    fn uncommitted_code_is_newest_and_an_uncommitted_entry_is_not_stale() {
        assert!(matches!(
            compare(&at(ZERO, 0, true), &at(A, 5, false)),
            SyncState::PossiblyStale { .. }
        ));
        assert_eq!(
            compare(&at(A, 5, false), &at(ZERO, 0, true)),
            SyncState::InSync
        );
        assert_eq!(
            compare(&at(ZERO, 0, true), &at(ZERO, 0, true)),
            SyncState::InSync
        );
    }

    #[test]
    fn civil_dates_are_right_across_leap_days_and_the_epoch() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(951_782_400), "2000-02-29");
        assert_eq!(civil_date(1_760_140_800), "2025-10-11");
    }

    #[test]
    fn a_command_that_outlives_its_timeout_is_killed_and_reported() {
        let started = Instant::now();
        let err = run(
            "sleep",
            &["30".to_string()],
            Path::new("."),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(err, BlameError::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_a_failure_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();
        let err = blame_ranges(dir.path(), "f.txt", &[(1, 1)], GIT_TIMEOUT).unwrap_err();
        assert!(matches!(err, BlameError::Failed(_)), "{err:?}");
    }

    /// A repository whose file was committed twice, the second time editing
    /// only its last line.
    fn history() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str], date: &str| {
            let out = std::process::Command::new("git")
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
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"], "2024-01-01T00:00:00Z");
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\n").unwrap();
        git(&["add", "f.txt"], "2024-01-01T00:00:00Z");
        git(
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "first"],
            "2024-01-01T00:00:00Z",
        );
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\nTHREE\n").unwrap();
        git(
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-am",
                "second",
            ],
            "2024-06-01T00:00:00Z",
        );
        dir
    }

    #[test]
    fn blame_gives_each_range_its_own_newest_commit() {
        let dir = history();
        let got =
            blame_ranges(dir.path(), "f.txt", &[(1, 2), (3, 3), (1, 3)], GIT_TIMEOUT).unwrap();
        assert_eq!(civil_date(got[0].time), "2024-01-01");
        assert_eq!(civil_date(got[1].time), "2024-06-01");
        assert_eq!(
            got[2], got[1],
            "the whole file is as new as its newest line"
        );
        assert!(!got[0].dirty);

        std::fs::write(dir.path().join("f.txt"), "one\ntwo\nTHREE\nfour\n").unwrap();
        let dirty = blame_ranges(dir.path(), "f.txt", &[(4, 4), (1, 1)], GIT_TIMEOUT).unwrap();
        assert!(dirty[0].dirty && !dirty[1].dirty);
        let past = blame_ranges(dir.path(), "f.txt", &[(40, 41)], GIT_TIMEOUT);
        assert!(past.is_err(), "a range past the end is not an answer");
    }

    #[test]
    fn the_cache_is_keyed_by_the_files_content_so_an_edit_is_seen() {
        let dir = history();
        let first = blame_ranges_cached(dir.path(), "f.txt", &[(3, 3)], GIT_TIMEOUT).unwrap();
        assert!(!first[0].dirty);
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\nEDITED\n").unwrap();
        let second = blame_ranges_cached(dir.path(), "f.txt", &[(3, 3)], GIT_TIMEOUT).unwrap();
        assert!(second[0].dirty, "a cached answer outlived an edit");
    }

    #[test]
    fn the_last_commit_of_a_file_is_the_coarse_answer() {
        let dir = history();
        let coarse = last_commit_of_file(dir.path(), "f.txt", GIT_TIMEOUT)
            .unwrap()
            .unwrap();
        assert_eq!(civil_date(coarse.time), "2024-06-01");
        assert!(last_commit_of_file(dir.path(), "nope.txt", GIT_TIMEOUT)
            .unwrap()
            .is_none());
    }
}
