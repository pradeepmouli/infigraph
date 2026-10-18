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

/// One commit that touched a range, newest first, with its patch limited to
/// the range's lines (headers removed, hunks kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub commit: String,
    /// Committer time, seconds since the epoch.
    pub time: i64,
    pub subject: String,
    pub patch: String,
}

/// What happened to a range of lines since a commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// Up to the `max_commits` asked for, newest first.
    pub commits: Vec<Change>,
    /// The range has older commits than the ones listed.
    pub more_commits: bool,
    /// Hunks of the working tree that touch the range and are not committed.
    pub uncommitted: Option<String>,
}

/// One hunk of `git diff -U0`, in old (base) and new (working tree) lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The new-side path (`+++ b/<file>`); empty when the diff names none.
    pub file: String,
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    text: String,
}

impl Hunk {
    /// The last new-side line, or the line a pure deletion sits after.
    pub fn new_end(&self) -> usize {
        if self.new_count == 0 {
            self.new_start
        } else {
            self.new_start + self.new_count - 1
        }
    }
}

/// `@@ -a[,b] +c[,d] @@` -> (a, b, c, d), a missing count being 1.
fn hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let span = |s: &str| -> Option<(usize, usize)> {
        match s.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = span(old)?;
    let (new_start, new_count) = span(new)?;
    Some((old_start, old_count, new_start, new_count))
}

/// The hunks of a unified diff, headers dropped, each naming its file. The one
/// parser of `git diff` output: `detect_changes`, `missing_docs` and this
/// module's own patches all read hunks through it.
pub fn parse_hunks(diff: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut in_header = false;
    let mut file = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_header = true;
        } else if in_header && line.starts_with("+++ ") {
            if let Some(path) = line.strip_prefix("+++ b/") {
                file = path.to_string();
            }
        } else if let Some((old_start, old_count, new_start, new_count)) = hunk_header(line) {
            in_header = false;
            hunks.push(Hunk {
                file: file.clone(),
                old_start,
                old_count,
                new_start,
                new_count,
                text: format!("{line}\n"),
            });
        } else if !in_header {
            if let Some(last) = hunks.last_mut() {
                last.text.push_str(line);
                last.text.push('\n');
            }
        }
    }
    hunks
}

/// The hunks that turn `base` into the working tree, committed or not.
pub fn diff_hunks(root: &Path, base: &str, timeout: Duration) -> Result<Vec<Hunk>, BlameError> {
    let diff = run(
        "git",
        &[
            "--no-pager",
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            base,
            "--",
        ]
        .map(String::from),
        root,
        timeout,
    )?;
    Ok(parse_hunks(&diff))
}

/// Whether the working-tree lines `range` and `hunk` share a line. A pure
/// deletion (`new_count` 0) sits between `new_start` and the next line.
fn touches(hunk: &Hunk, range: (usize, usize)) -> bool {
    if hunk.new_count == 0 {
        hunk.new_start >= range.0.saturating_sub(1) && hunk.new_start <= range.1
    } else {
        hunk.new_start <= range.1 && hunk.new_start + hunk.new_count > range.0
    }
}

/// The lines of HEAD that the working-tree lines `range` came from: the range
/// shrunk past lines edited and not committed, and shifted by every hunk above
/// it. `None` when every line of it is uncommitted.
fn head_range(hunks: &[Hunk], range: (usize, usize)) -> Option<(usize, usize)> {
    let edited = |line: usize| {
        hunks
            .iter()
            .any(|h| h.new_count > 0 && (h.new_start..h.new_start + h.new_count).contains(&line))
    };
    // A hunk is above `line` when it ends before it; a pure deletion sits
    // after line `new_start`.
    let above = |h: &Hunk, line: usize| {
        if h.new_count == 0 {
            h.new_start < line
        } else {
            h.new_start + h.new_count <= line
        }
    };
    let shifted = |line: usize| -> usize {
        let delta: isize = hunks
            .iter()
            .filter(|h| above(h, line))
            .map(|h| h.old_count as isize - h.new_count as isize)
            .sum();
        (line as isize + delta).max(1) as usize
    };
    let first = (range.0..=range.1).find(|l| !edited(*l))?;
    let last = (first..=range.1).rev().find(|l| !edited(*l))?;
    Some((shifted(first), shifted(last)))
}

/// Context lines kept on each side of a change in a commit's patch.
const PATCH_CONTEXT: usize = 1;

/// `git log -L` prints every line of the tracked range, changed or not. Keeps
/// each hunk's header, its `+`/`-` lines and [`PATCH_CONTEXT`] lines around
/// them, and marks what it left out with `...`, so a reader sees what changed
/// instead of the whole function again.
fn squeeze(hunks: &str) -> String {
    let lines: Vec<&str> = hunks.lines().collect();
    let is_change = |l: &str| l.starts_with('+') || l.starts_with('-');
    let mut show = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("@@") {
            show[i] = true;
        } else if is_change(line) {
            let from = i.saturating_sub(PATCH_CONTEXT);
            let to = (i + PATCH_CONTEXT).min(lines.len() - 1);
            for (j, shown) in show.iter_mut().enumerate().take(to + 1).skip(from) {
                // A hunk header stays a header: context never reaches across it.
                *shown |= !lines[j].starts_with("@@") || j == i;
            }
        }
    }
    let mut out = String::new();
    let mut skipped = false;
    for (line, shown) in lines.iter().zip(&show) {
        if *shown {
            if skipped && !line.starts_with("@@") {
                out.push_str("...\n");
            }
            skipped = false;
            out.push_str(line);
            out.push('\n');
        } else {
            skipped = true;
        }
    }
    out
}

/// Marks the start of a commit in the `git log` format below; no subject or
/// patch line starts with it.
const COMMIT_MARK: char = '\u{1}';

/// What changed in the lines `lines` of `file` (as they are in the working
/// tree) since commit `since`: the commits after it that touched them, newest
/// first and at most `max_commits`, each with its patch limited to those lines
/// (`git log -L`, which follows the lines as they move), plus the working
/// tree's own uncommitted edits to them.
///
/// `git log -L` reads line numbers at HEAD, so a file with uncommitted edits
/// is mapped back first. A file git cannot follow (not tracked, absent at
/// `since`) is an error: the caller says "unknown", never an empty history.
pub fn changes_since(
    root: &Path,
    file: &str,
    lines: (usize, usize),
    since: &str,
    max_commits: usize,
    timeout: Duration,
) -> Result<Changes, BlameError> {
    let diff = run(
        "git",
        &[
            "--no-pager",
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "HEAD",
            "--",
            file,
        ]
        .map(String::from),
        root,
        timeout,
    )?;
    let hunks = parse_hunks(&diff);
    let uncommitted: String = hunks
        .iter()
        .filter(|h| touches(h, lines))
        .map(|h| h.text.as_str())
        .collect();
    let mut changes = Changes {
        uncommitted: (!uncommitted.is_empty()).then_some(uncommitted),
        ..Changes::default()
    };

    let Some((start, end)) = head_range(&hunks, lines) else {
        return Ok(changes);
    };
    // `git log -L` fails on an empty revision range ("No commit specified?"),
    // which is the ordinary case of an entry written in HEAD itself.
    let after = run(
        "git",
        &["--no-pager", "rev-list", "-n1"]
            .map(String::from)
            .into_iter()
            .chain([format!("{since}..HEAD")])
            .collect::<Vec<_>>(),
        root,
        timeout,
    )?;
    if after.trim().is_empty() {
        return Ok(changes);
    }
    let args: Vec<String> = vec![
        "--no-pager".into(),
        "log".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "-U1".into(),
        format!("-n{}", max_commits + 1),
        format!("--format={COMMIT_MARK}%H %ct %s"),
        format!("-L{start},{end}:{file}"),
        format!("{since}..HEAD"),
    ];
    let log = run("git", &args, root, timeout)?;
    for block in log.split(COMMIT_MARK).skip(1) {
        let (head, patch) = block.split_once('\n').unwrap_or((block, ""));
        let mut fields = head.splitn(3, ' ');
        let (Some(commit), Some(time), subject) = (
            fields.next(),
            fields.next().and_then(|t| t.parse().ok()),
            fields.next().unwrap_or(""),
        ) else {
            continue;
        };
        let patch: String = parse_hunks(patch)
            .iter()
            .map(|h| squeeze(&h.text))
            .collect();
        changes.commits.push(Change {
            commit: commit.to_string(),
            time,
            subject: subject.to_string(),
            patch,
        });
    }
    if changes.commits.len() > max_commits {
        changes.commits.truncate(max_commits);
        changes.more_commits = true;
    }
    Ok(changes)
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

    fn hunk(old: (usize, usize), new: (usize, usize)) -> Hunk {
        Hunk {
            file: String::new(),
            old_start: old.0,
            old_count: old.1,
            new_start: new.0,
            new_count: new.1,
            text: String::new(),
        }
    }

    #[test]
    fn hunks_name_their_file_and_a_deleted_file_names_none() {
        let diff = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1,0 +2,3 @@\n+x\n+y\n+z\ndiff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\n--- a/gone.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n";
        let got = parse_hunks(diff);
        assert_eq!(got.len(), 2);
        assert_eq!(
            (got[0].file.as_str(), got[0].new_start, got[0].new_end()),
            ("a.rs", 2, 4)
        );
        assert_eq!(
            got[1].file, "a.rs",
            "a deleted file's hunk is not given a path of its own"
        );
        assert_eq!((got[1].new_start, got[1].new_count), (0, 0));
    }

    #[test]
    fn diff_hunks_covers_commits_and_uncommitted_edits_since_the_base() {
        let dir = history();
        let base = last_commit_of_file(dir.path(), "f.txt", GIT_TIMEOUT)
            .unwrap()
            .unwrap()
            .commit;
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\nEDITED\nfour\nfive\n").unwrap();
        let got = diff_hunks(dir.path(), &base, GIT_TIMEOUT).unwrap();
        assert!(
            !got.is_empty() && got.iter().all(|h| h.file == "f.txt"),
            "{got:?}"
        );
        assert!(diff_hunks(dir.path(), "no-such-ref", GIT_TIMEOUT).is_err());
    }

    #[test]
    fn hunk_headers_default_a_missing_count_to_one() {
        assert_eq!(hunk_header("@@ -3 +3,2 @@ fn x"), Some((3, 1, 3, 2)));
        assert_eq!(hunk_header("@@ -10,0 +11,4 @@"), Some((10, 0, 11, 4)));
        assert_eq!(hunk_header("not a header"), None);
    }

    #[test]
    fn a_diffs_headers_are_dropped_and_its_hunks_kept_whole() {
        let hunks = parse_hunks(
            "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -2 +2 @@\n-two\n+TWO\n@@ -9,0 +10 @@\n+ten\n",
        );
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].text, "@@ -2 +2 @@\n-two\n+TWO\n");
        assert_eq!(hunks[1].text, "@@ -9,0 +10 @@\n+ten\n");
    }

    #[test]
    fn working_tree_lines_map_back_to_head_past_every_hunk_above_them() {
        // Two lines inserted at 2-3, then line 10 edited, then a line deleted
        // after line 19 (as `git diff -U0` numbers it: the line before).
        let hunks = [
            hunk((1, 0), (2, 2)),
            hunk((8, 1), (10, 1)),
            hunk((18, 1), (19, 0)),
        ];
        assert_eq!(
            head_range(&hunks, (5, 7)),
            Some((3, 5)),
            "below the insertion"
        );
        assert_eq!(
            head_range(&hunks, (9, 11)),
            Some((7, 9)),
            "line 10 is uncommitted"
        );
        assert_eq!(
            head_range(&hunks, (10, 10)),
            None,
            "all of it is uncommitted"
        );
        assert_eq!(
            head_range(&hunks, (20, 22)),
            Some((19, 21)),
            "below the deleted line"
        );
        assert_eq!(
            head_range(&[], (4, 6)),
            Some((4, 6)),
            "a clean file maps to itself"
        );
    }

    /// `history()` plus a third commit that rewrites line 1, then one that
    /// edits line 3, so line 3 has two commits after the first.
    fn longer_history() -> tempfile::TempDir {
        let dir = history();
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
        std::fs::write(dir.path().join("f.txt"), "ONE\ntwo\nTHREE\n").unwrap();
        git(
            &["commit", "-q", "-am", "third: line one"],
            "2024-07-01T00:00:00Z",
        );
        std::fs::write(dir.path().join("f.txt"), "ONE\ntwo\nthree again\n").unwrap();
        git(
            &["commit", "-q", "-am", "fourth: line three"],
            "2024-08-01T00:00:00Z",
        );
        dir
    }

    fn first_commit(dir: &Path) -> String {
        let out = run(
            "git",
            &[
                "rev-list".to_string(),
                "--max-parents=0".to_string(),
                "HEAD".to_string(),
            ],
            dir,
            GIT_TIMEOUT,
        )
        .unwrap();
        out.trim().to_string()
    }

    #[test]
    fn only_the_commits_that_touched_the_lines_are_listed_newest_first_with_their_lines() {
        let dir = longer_history();
        let first = first_commit(dir.path());

        let got = changes_since(dir.path(), "f.txt", (3, 3), &first, 5, GIT_TIMEOUT).unwrap();
        let subjects: Vec<&str> = got.commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["fourth: line three", "second"], "{got:#?}");
        assert!(got.commits[0].patch.contains("+three again"), "{got:#?}");
        assert!(got.commits[0].patch.contains("-THREE"), "{got:#?}");
        assert!(
            !got.commits[0].patch.contains("diff --git"),
            "headers stay out"
        );
        assert!(!got.more_commits && got.uncommitted.is_none());

        let other = changes_since(dir.path(), "f.txt", (2, 2), &first, 5, GIT_TIMEOUT).unwrap();
        assert!(
            other.commits.is_empty(),
            "line two never changed: {other:#?}"
        );
    }

    #[test]
    fn the_number_of_commits_is_capped_and_the_cap_is_reported() {
        let dir = longer_history();
        let first = first_commit(dir.path());
        let got = changes_since(dir.path(), "f.txt", (3, 3), &first, 1, GIT_TIMEOUT).unwrap();
        assert_eq!(got.commits.len(), 1);
        assert!(got.more_commits);
    }

    #[test]
    fn an_entry_written_at_head_has_no_history_after_it_and_is_not_an_error() {
        let dir = longer_history();
        let head = run(
            "git",
            &["rev-parse".to_string(), "HEAD".to_string()],
            dir.path(),
            GIT_TIMEOUT,
        )
        .unwrap();
        let got = changes_since(dir.path(), "f.txt", (1, 3), head.trim(), 5, GIT_TIMEOUT).unwrap();
        assert_eq!(got, Changes::default());
    }

    #[test]
    fn uncommitted_edits_are_reported_and_the_log_is_read_at_the_right_lines() {
        let dir = longer_history();
        let first = first_commit(dir.path());
        // Two lines go in above line 3, and line 3 itself is edited again.
        std::fs::write(
            dir.path().join("f.txt"),
            "new a\nnew b\nONE\ntwo\nthree edited\n",
        )
        .unwrap();

        // Line 4 ("two") never changed; it moved down by two.
        let moved = changes_since(dir.path(), "f.txt", (4, 4), &first, 5, GIT_TIMEOUT).unwrap();
        assert!(
            moved.commits.is_empty() && moved.uncommitted.is_none(),
            "{moved:#?}"
        );

        // The last line: its committed history is found at its HEAD line, and
        // the edit on top of it is listed apart.
        let edited = changes_since(dir.path(), "f.txt", (3, 5), &first, 5, GIT_TIMEOUT).unwrap();
        assert!(
            edited
                .uncommitted
                .as_deref()
                .is_some_and(|u| u.contains("+three edited")),
            "{edited:#?}"
        );
        assert!(
            edited
                .commits
                .iter()
                .any(|c| c.subject == "third: line one"),
            "the committed history of the unedited lines is lost: {edited:#?}"
        );
    }

    #[test]
    fn a_file_git_does_not_track_is_an_error_not_an_empty_history() {
        let dir = longer_history();
        let first = first_commit(dir.path());
        std::fs::write(dir.path().join("loose.txt"), "x\n").unwrap();
        assert!(changes_since(dir.path(), "loose.txt", (1, 1), &first, 5, GIT_TIMEOUT).is_err());
    }

    #[test]
    fn a_patch_keeps_its_changes_and_one_line_around_them_and_marks_the_rest() {
        let hunk = "@@ -1,9 +1,9 @@ fn f\n a\n b\n c\n-d\n+D\n e\n f\n g\n h\n-i\n+I\n";
        assert_eq!(
            squeeze(hunk),
            "@@ -1,9 +1,9 @@ fn f\n...\n c\n-d\n+D\n e\n...\n h\n-i\n+I\n"
        );
        // Nothing to cut: unchanged.
        assert_eq!(squeeze("@@ -1 +1 @@\n-x\n+y\n"), "@@ -1 +1 @@\n-x\n+y\n");
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
