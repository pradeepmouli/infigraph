use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kuzu::{Connection, Database, SystemConfig};

use super::schema::{CREATE_SCHEMA, MIGRATIONS};
use super::store_util::escape;
use crate::lockfile::{self, LockFile};

/// RAII guard for exclusive write access to the graph store.
/// Holds an advisory file lock on `<db_path>.lock` with an identity
/// payload (see `crate::lockfile`).
#[derive(Debug)]
pub struct WriteLock {
    _guard: LockFile,
}

/// Role string stamped into the graph write lock's identity payload.
const GRAPH_WRITE_ROLE: &str = "graph-write";

/// Default wait budget for the graph write lock. Individual write calls
/// are short; 30s of waiting means something is wedged — surface it.
const GRAPH_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Wait budget for `open_read_only_or_degrade`'s lock-then-recheck step
/// before quarantining a dead-holder graph. Shorter than
/// `GRAPH_WRITE_TIMEOUT`: this runs on read paths (MCP tool calls, CLI
/// reads) that callers expect to be responsive, and a concurrent rebuild
/// finishing within this window is the only case that changes the outcome
/// -- past it, quarantining under the original (pre-lock) evidence is
/// still correct, just delayed by however long the wait took.
const QUARANTINE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl WriteLock {
    fn acquire(lock_path: &Path) -> Result<Self> {
        Self::acquire_with_timeout(lock_path, GRAPH_WRITE_TIMEOUT)
    }

    fn acquire_with_timeout(lock_path: &Path, timeout: std::time::Duration) -> Result<Self> {
        let guard = lockfile::acquire(lock_path, GRAPH_WRITE_ROLE, timeout)?;
        Ok(Self { _guard: guard })
    }

    fn try_acquire(lock_path: &Path) -> Result<Option<Self>> {
        Ok(lockfile::try_acquire(lock_path, GRAPH_WRITE_ROLE)?.map(|guard| Self { _guard: guard }))
    }
}

/// Marker error: the graph's on-disk state failed integrity recovery (e.g.
/// WAL replay). Downcast target for callers that route to quarantine
/// (DESIGN-hardening.md R3.1) instead of scanning damaged pages.
///
/// Exists because a tolerant read-only open used to serve a torn base image
/// over a corrupt WAL instead of erroring — the 2026-07-19 incident stayed
/// latent for a day until scans over the torn graph segfaulted.
#[derive(Debug)]
pub struct GraphCorruption {
    pub detail: String,
}

impl std::fmt::Display for GraphCorruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "graph corruption detected: {}", self.detail)
    }
}

impl std::error::Error for GraphCorruption {}

/// Reason a read was served from something other than the live graph.
#[derive(Debug, Clone)]
pub enum DegradeReason {
    /// The live graph had a dead-holder WAL and was quarantined; this read
    /// was served from the most recent cleanly-retired graph instead, while
    /// a rebuild has been signaled to the daemon coordinator in the
    /// background. Callers should surface a staleness banner naming
    /// `snapshot_path`'s age.
    PreCrashSnapshot {
        snapshot_path: PathBuf,
        dead_pid: u32,
    },
}

/// Minimum plausible size of a Kuzu database file. A freshly created
/// database is at least one page (4 KiB); anything smaller is a
/// truncated/corrupt file that Kuzu's own parser cannot be trusted with.
const MIN_DB_FILE_SIZE: u64 = 4096;

/// Preflight check before handing a path to `kuzu::Database::new`.
///
/// A truncated or corrupt database file can make Kuzu's parser read a bogus
/// size field and request a huge allocation, which aborts the whole process
/// on some platforms (observed on Linux) or segfaults later at read time
/// (observed on macOS) — before any `Result` exists to catch it. Rejecting
/// obviously-invalid files here turns that abort into a normal error that
/// callers' wipe-and-rebuild recovery (`Infigraph::init`, `DocIndex::init`)
/// already handles.
///
/// A missing path (fresh create) and a directory (legacy on-disk layout)
/// are both fine.
pub fn validate_db_file(path: &Path) -> Result<()> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        // Doesn't exist yet — fresh create.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        // Permission errors etc. are real problems — don't mask them as
        // "fresh create" or Kuzu will fail later with a worse message.
        Err(e) => {
            return Err(e)
                .with_context(|| format!("read database metadata for {}", path.display()));
        }
    };
    if meta.is_dir() {
        return Ok(()); // legacy directory layout — let Kuzu handle it
    }
    if meta.len() < MIN_DB_FILE_SIZE {
        anyhow::bail!(
            "database file {} is truncated/corrupt ({} bytes, expected at least {})",
            path.display(),
            meta.len(),
            MIN_DB_FILE_SIZE
        );
    }
    Ok(())
}

/// Every WAL-family sibling of the database at `db_path` that currently
/// exists: `<db>.wal` plus the `<db>.wal.*` family.
///
/// Kuzu's on-disk WAL filename APPENDS ".wal" to the full db filename
/// (e.g. "graph" -> "graph.wal", "docs.kuzu" -> "docs.kuzu.wal"). It does
/// NOT replace the extension the way `Path::with_extension` does --
/// `db_path.with_extension("wal")` on an extensioned path like "docs.kuzu"
/// silently computes "docs.wal", a file Kuzu never wrote. Verified
/// empirically (see the Task 5 report on `wipe_graph`) that the real
/// sibling is always `<full filename>.wal`.
pub fn wal_family_paths(db_path: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let wal = PathBuf::from(format!("{}.wal", db_path.display()));
    if wal.exists() {
        found.push(wal);
    }
    if let (Some(parent), Some(name)) = (db_path.parent(), db_path.file_name()) {
        let prefix = format!("{}.wal.", name.to_string_lossy());
        if let Ok(entries) = std::fs::read_dir(parent) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    found.push(e.path());
                }
            }
        }
    }
    found
}

/// Delete every WAL-family sibling of the database at `db_path`.
///
/// Kuzu leaves WAL-family temp siblings (e.g. `<db>.wal.checkpoint`)
/// carrying the OLD database's ID; a leftover one makes a freshly recreated
/// database at the same path *permanently unopenable* ("Database ID ... does
/// not match"). Anything that recreates a database at a path some other
/// database previously occupied must clear the whole family first, or that
/// path is wedged until a human deletes the leftovers by hand.
///
/// Best-effort by design: a sibling that can't be removed is not worth
/// failing the caller over, and the caller's own error handling covers the
/// resulting open failure.
pub fn remove_wal_family(db_path: &Path) {
    for path in wal_family_paths(db_path) {
        let _ = std::fs::remove_file(path);
    }
}

/// The advisory lock path for the database at `db_path`.
///
/// APPENDS ".lock" to the full db filename, the same convention used for
/// the WAL family above (see `wal_family_paths`'s doc comment) -- and for
/// the identical reason: `db_path.with_extension("lock")` *replaces* an
/// existing extension rather than appending, so on an extensioned path like
/// "docs.kuzu" it silently computes "docs.lock" (colliding with an
/// unrelated file) instead of "docs.kuzu.lock", and on a path like
/// "graph.rebuilding" it computes "graph.lock" -- the *same* lock path as
/// the live "graph" database, rather than a lock of its own. For
/// infigraph's extensionless production path (".infigraph/graph"),
/// `with_extension` and append coincide, so this only changes behavior for
/// extensioned db paths.
pub fn db_lock_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", db_path.display()))
}

/// If `db_path`'s graph shows signs of an unclean shutdown that could make
/// Kuzu's WAL-replay-on-open crash the whole process (R3.1.3,
/// docs/DESIGN-hardening.md §3.1, github.com/pradeepmouli/infigraph#92),
/// returns the dead holder's PID. `None` means it's safe to proceed to
/// `Database::new` as normal.
///
/// Two signals together, deliberately not either alone:
/// - A WAL-family sibling exists (`wal_family_paths`) -- Kuzu did not
///   complete a clean checkpoint before this graph was last closed. This
///   alone is completely routine: a live writer mid-transaction, or a
///   replay that would succeed fine, both leave a WAL sibling in place.
/// - The write lock's recorded holder is confirmed dead (the OS reports no
///   such process running right now). This is what turns "needs replay"
///   (routine) into "the process that would have driven that
///   replay/checkpoint died before finishing it" (suspect) -- observed
///   directly (2026-08-20, three matching macOS crash reports) to crash
///   Kuzu's WAL replay with `SIGBUS` on exactly this on-disk state.
///
/// Requiring both keeps this from flagging the common, harmless case (a
/// live writer's WAL, or a replay that would just work) while still
/// catching the exact scenario that caused #92. A lock file that's absent,
/// empty (cleanly released), or unparseable reads as "can't confirm a dead
/// holder" via `lockfile::read_holder` returning `None`, and does not flag
/// -- conservative by design, since a false positive here means refusing
/// to open a perfectly good graph.
/// Decide whether a failed read-only open means the graph is *damaged* or
/// merely *busy*.
///
/// `open_read_only` refuses a dead-holder WAL before it ever reaches
/// `Database::new`, so a WAL-related failure past that point means the lock
/// holder is either absent or ALIVE. A live writer's uncheckpointed WAL
/// simply is not readable from outside -- a concurrency artifact, not damage.
///
/// This used to return `GraphCorruption` for *any* message mentioning a WAL,
/// while `is_transient_wal_open_race_error` tested essentially the same
/// condition and called it retryable: two predicates, one input, opposite
/// verdicts. Because `GraphCorruption` is the downcast target that routes to
/// quarantine, the corruption reading also meant `infigraph verify` told the
/// user to rebuild a perfectly healthy graph. Reproduced on sittir
/// 2026-09-02: an idle daemon holding a 690 KB WAL produced "Corrupted wal
/// file. Read out invalid WAL record type." on three consecutive runs;
/// stopping that daemon checkpointed the WAL away and the same untouched
/// file verified 4 PASS / 0 WARN / 0 FAIL.
///
/// The busy wording keeps the "failed to open kuzu db" prefix so
/// `is_transient_wal_open_race_error` still recognises it as retryable.
fn classify_read_only_open_failure(msg: String, live_holder: Option<u32>) -> anyhow::Error {
    if !msg.to_lowercase().contains("wal") {
        return anyhow::anyhow!(msg);
    }
    match live_holder {
        Some(pid) => anyhow::anyhow!(
            "{msg} -- the graph is held by a live writer (PID {pid}) whose WAL has not been \
             checkpointed yet, so it cannot be read from outside. This is not corruption: \
             retry once the writer is idle (`infigraph ps` / `infigraph watch-status`)."
        ),
        None => anyhow::Error::new(GraphCorruption { detail: msg }),
    }
}

/// How a dead WAL holder died, phrased for the refusal message (#146).
///
/// A deliberate hard exit mid-write manufactures exactly the dead-holder
/// state the R3.1.3 guard refuses, and the guard cannot tell it apart from an
/// unexplained death. The rebuild is required either way -- the WAL is still
/// unreplayable -- but "the daemon shut itself down mid-import" reads very
/// differently from "your graph may be corrupt". Empty when nothing was
/// recorded, or when the record names a different process.
fn hard_exit_explanation(db_path: &Path, pid: u32) -> String {
    let Some(exit) = db_path
        .parent()
        .and_then(|dir| crate::recovery::unclean_exit_for_pid(dir, pid))
    else {
        return String::new();
    };
    match exit.phase {
        Some(phase) => format!(
            " -- that process hard-exited on purpose ({}) while writing {phase}, so this is an \
             interrupted write rather than damage",
            exit.reason
        ),
        None => format!(
            " -- that process hard-exited on purpose ({}), so this is an interrupted write \
             rather than damage",
            exit.reason
        ),
    }
}

/// Buffer-pool budget for a read-only open.
///
/// `SystemConfig`'s default is 0, meaning "auto-detect" -- a fraction of
/// system RAM, sized for a long-lived database. Read handles here are the
/// opposite: short-lived and opened per read, because a held read-only
/// `Database` never observes another process's later commits and so cannot
/// be reused (see `DaemonKuzuBackend::open_read`; measured directly -- a
/// cached handle saw 2 distinct values of a symbol the writer renamed on
/// every iteration, where per-read reopening saw 104-121).
///
/// lbug treats `Database` instances as a bounded resource for exactly this
/// reason: its own test config lowers `max_db_size` with the note that it
/// "limits the number of databases which can be open in a single process".
/// Leaving every short-lived reader to claim an auto-sized pool is the same
/// resource class as the "Buffer manager exception: Mmap for size ... failed"
/// failures seen under parallel test runs, and as the buffer-pool exhaustion
/// that killed a daemon before the 0.20.2 bump.
///
/// 256MB is a budget, not a cap on graph size: the pool is a cache over the
/// file, so a larger graph still reads correctly, just with more I/O once
/// the working set exceeds it. The largest graphs on this machine are
/// ~160MB, so a realistic working set still fits entirely.
const READ_ONLY_BUFFER_POOL_BYTES: u64 = 256 * 1024 * 1024;

/// PID currently holding `lock_path`, if that process is still running.
///
/// The complement of the dead-holder case `unclean_shutdown_wal_holder`
/// reports: used to tell "a writer is mid-transaction" apart from "the graph
/// is damaged" when a read-only open fails.
pub fn live_lock_holder(lock_path: &Path) -> Option<u32> {
    let holder = lockfile::read_holder(lock_path)?;
    lockfile::holder_is_alive(&holder).then_some(holder.pid)
}

/// Pick a live writer from the two signals available, preferring the lock
/// payload and falling back to whoever actually has the database file open.
///
/// Pure so the decision is testable without a live writer to arrange.
/// `file_holders` must already exclude this process and non-infigraph pids --
/// see [`live_graph_writer`].
fn live_graph_writer_from(payload_holder: Option<u32>, file_holders: &[u32]) -> Option<u32> {
    payload_holder.or_else(|| file_holders.first().copied())
}

/// PID of a live writer of `db_path`, if there is one.
///
/// The lock payload is the precise signal, but it is not the only state a
/// live writer can be in. A daemon keeps the database open across its whole
/// session while taking `graph.lock` only around write transactions, and the
/// lockfile blanks the payload on release -- so between transactions the file
/// is open, the WAL is uncheckpointed, and the payload is empty.
///
/// That third state used to read as "no live holder", which
/// [`classify_read_only_open_failure`] turns into `GraphCorruption`: a
/// healthy, actively-served graph reported as damaged. It is the same
/// symptom d6a742f fixed for the payload-names-a-pid case, recurring through
/// the one door that fix left open -- observed live on 2026-09-04, where
/// `lsof` showed the daemon holding a 135MB graph with a 1.2MB uncheckpointed
/// WAL while `graph.lock` was zero bytes.
///
/// `pids_holding_file` is best-effort and returns empty for "unknown" as well
/// as "nobody" (it has no implementation outside Linux/macOS), so this can
/// only ever add confidence that a writer exists -- never prove one absent.
/// Where it cannot tell, behaviour is exactly as before.
fn live_graph_writer(db_path: &Path, lock_path: &Path) -> Option<u32> {
    let payload_holder = live_lock_holder(lock_path);
    if payload_holder.is_some() {
        return payload_holder;
    }
    let me = std::process::id();
    let holders: Vec<u32> = crate::ps::pids_holding_file(db_path)
        .into_iter()
        .filter(|&pid| pid != me && crate::ps::is_infigraph_process(pid))
        .collect();
    live_graph_writer_from(payload_holder, &holders)
}

pub fn unclean_shutdown_wal_holder(db_path: &Path, lock_path: &Path) -> Option<u32> {
    if wal_family_paths(db_path).is_empty() {
        return None;
    }
    let holder = lockfile::read_holder(lock_path)?;
    if lockfile::holder_is_alive(&holder) {
        return None; // holder is alive -- not our call to intervene
    }
    Some(holder.pid)
}

/// Reads the schema version stamped in `db`'s GraphMeta singleton. 0 =
/// pre-versioning database (missing table/row/column all read as 0).
fn read_stored_schema_version(db: &Database) -> i64 {
    let Ok(conn) = Connection::new(db) else {
        return 0;
    };
    let Ok(mut result) =
        conn.query("MATCH (g:GraphMeta {id: 'singleton'}) RETURN g.schema_version")
    else {
        return 0;
    };
    result
        .next()
        .and_then(|row| row.first().and_then(|v| v.to_string().parse::<i64>().ok()))
        .unwrap_or(0)
}

/// R8.1 (#85): refuse to touch a database written by a NEWER schema than
/// this binary understands -- "open-and-guess" against a future layout is
/// how silent corruption starts. Runs BEFORE any DDL, so the refusal is
/// side-effect-free. Older/unstamped versions return Ok: the unconditional
/// CREATE/MIGRATIONS pass migrates them forward, exactly as before.
fn refuse_newer_schema(db: &Database, db_path: &Path) -> Result<()> {
    let stored = read_stored_schema_version(db);
    if stored > super::schema::SCHEMA_VERSION {
        anyhow::bail!(
            "graph {} was written by a newer infigraph (schema v{stored}; this binary \
             understands up to v{}) -- upgrade infigraph rather than opening it with \
             this version",
            db_path.display(),
            super::schema::SCHEMA_VERSION
        );
    }
    Ok(())
}

// ---- Open-failure classification (R3.1.1) ----
//
// `Database::new` collapses every failure into one stringified error before
// it reaches any caller, so the only way to tell "not corruption -- leave the
// file alone" from "durably unopenable" is the text. These are the classes
// every wipe-on-open-failure path (`Infigraph::init`, `DocIndex::init`,
// `open_combined_graph`, the combined-docs generation) must recognise BEFORE
// taking a destructive branch. They live here, next to the open itself,
// precisely so that each of those paths does not grow its own partial list
// (#143 found two that had none at all).

/// Kuzu's IO-layer error for "another process holds this database's lock"
/// (see docs.ladybugdb.com/concurrency) is lock contention, not corruption.
/// This was previously indistinguishable from genuine corruption, so a
/// second `infigraph` process opening a graph while a watcher already had
/// it open would trigger `wipe_graph`, destroying the watcher's live data.
pub fn is_lock_contention_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("Could not set lock on file")
}

/// A `Database::new` open failure (`"failed to open kuzu db"` /
/// `"failed to open kuzu db (read-only)"`, `GraphStore::open`/`open_read_only`'s
/// own wrapping) that mentions the WAL. Distinct from `unclean_shutdown_wal_holder`'s
/// deliberately different-worded dead-holder bail ("...unreplayed WAL from
/// process {pid}, which is no longer running...", raised *before* `Database::new`
/// is ever called) -- that one is genuine, already-diagnosed corruption and must
/// not retry.
///
/// A live writer actively checkpointing/rotating the WAL can make a
/// concurrent read-only open transiently fail this way (observed: "Runtime
/// exception: Corrupted wal file. Read out invalid WAL record type." against
/// a graph `doctor` calls healthy moments later, with a live write-lock
/// holder) -- retrying briefly lets that resolve instead of hard-failing
/// (or, on the write path, escalating straight to a wipe-and-rebuild) on
/// what may just be a race.
pub fn is_transient_wal_open_race_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("failed to open kuzu db") && msg.to_lowercase().contains("wal")
}

/// lbug's `DatabaseHeader` refusal for a data file stamped with a storage
/// version this build cannot read ("Trying to read a database file with a
/// different version. Database file version: N, Current build storage
/// version: M"). `Database::new` raises it on any non-empty file (via
/// `Checkpointer::readCheckpoint`), so it arrives through the same "failed
/// to open ... kuzu db" wrapping as every other open failure. It is neither
/// transient nor corruption: the database was written by an `infigraph` on
/// a different lbug version (typically a stale binary still on the old one,
/// or a file left behind by a newer install) and this process simply cannot
/// read it. Wiping it would destroy a perfectly good database and serve an
/// empty one back as healthy (#140).
pub fn is_storage_version_mismatch_error(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("Trying to read a database file with a different version")
}

/// Every open failure that must never trigger a wipe or quarantine: the
/// file is either busy, mid-race with a live writer, or written on a lbug
/// version this build cannot read. The single guard for callers whose only
/// choices are "refuse" or "destroy and rebuild" (`DocIndex::init`,
/// `open_combined_graph`); `Infigraph::init` distinguishes the classes
/// itself because it retries the transient one.
pub fn open_failure_is_not_corruption(err: &anyhow::Error) -> bool {
    is_transient_open_error(err) || is_storage_version_mismatch_error(err)
}

/// lbug refuses a read-only open while a writer's checkpoint is running
/// ("Cannot open database in read-only mode while checkpoint is in
/// progress. Please retry later.") -- its own retry-later signal. Seen on
/// sittir (2026-09-02) right after the daemon accepted a write: every
/// post-write read-only open in `infigraph index` failed on it, including
/// the registration step.
pub fn is_checkpoint_in_progress_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("checkpoint is in progress")
}

/// The one list of open failures worth retrying: another process holds the
/// file lock, a live writer is mid-checkpoint, or the WAL is being rotated
/// under a concurrent open. Every retry loop and every "is this corruption?"
/// decision consults this, never its parts.
pub fn is_transient_open_error(err: &anyhow::Error) -> bool {
    is_lock_contention_error(err)
        || is_checkpoint_in_progress_error(err)
        || is_transient_wal_open_race_error(err)
}

/// Context line for a lock-contention open failure, naming the holder
/// when the OS can tell us (see `lock_probe::describe_lock_holder`).
pub fn lock_contention_context(db_path: &Path) -> String {
    format!(
        "{} is locked by another infigraph process ({}) -- not corrupted, so it was left \
         untouched. Run `infigraph ps` / `infigraph watch-status` or try again in a moment.",
        db_path.display(),
        super::lock_probe::describe_lock_holder(db_path, super::lock_probe::ProbeFor::Write)
    )
}

/// Context line for a storage-version refusal. lbug's own message (kept as
/// the cause) already names both versions; this adds which build is
/// speaking, that nothing was touched, and the two ways out.
pub fn storage_version_mismatch_context(db_path: &Path) -> String {
    format!(
        "{} was written on a different lbug storage version than this build (v{}) can read \
         -- not corrupted, so it was left untouched. Every process touching this project must \
         run the same installed infigraph build (`infigraph doctor` / `infigraph ps` show mixed \
         builds); to rebuild on this build's version instead, run `infigraph index --full`.",
        db_path.display(),
        kuzu::get_storage_version()
    )
}

/// Context line for any [`open_failure_is_not_corruption`] failure, so a
/// caller that refuses instead of wiping explains *why* the file was left
/// alone in the same words `Infigraph::init` uses.
pub fn non_corruption_open_context(err: &anyhow::Error, db_path: &Path) -> String {
    if is_lock_contention_error(err) {
        lock_contention_context(db_path)
    } else if is_checkpoint_in_progress_error(err) {
        format!(
            "{} is being checkpointed by a live writer -- not corrupted, so it was left \
             untouched; retry in a moment.",
            db_path.display()
        )
    } else if is_storage_version_mismatch_error(err) {
        storage_version_mismatch_context(db_path)
    } else {
        format!(
            "{} failed to open on what looks like a transient WAL race with a live writer -- \
             not corrupted, so it was left untouched; try again in a moment",
            db_path.display()
        )
    }
}

/// Persistent graph store backed by Kuzu.
pub struct GraphStore {
    db: Database,
    lock_path: PathBuf,
}

impl GraphStore {
    /// Open or create a Kuzu database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_lock_timeout(path, GRAPH_WRITE_TIMEOUT)
    }

    /// Open with a caller-chosen wait budget for the schema-init write lock.
    pub fn open_with_lock_timeout(path: &Path, timeout: std::time::Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        validate_db_file(path)?;
        let lock_path = db_lock_path(path);
        if let Some(pid) = unclean_shutdown_wal_holder(path, &lock_path) {
            let explanation = hard_exit_explanation(path, pid);
            anyhow::bail!(
                "graph {} has an unreplayed WAL from process {pid}, which is no longer \
                 running (unclean shutdown){explanation} -- refusing to open it directly since \
                 WAL replay in this state has crashed the whole process before (see \
                 github.com/pradeepmouli/infigraph#92); run `infigraph index --full` to rebuild",
                path.display()
            );
        }
        let db = {
            // Opening replays the WAL and may checkpoint -- transaction-
            // manager code with no Rust frame above it to catch anything.
            let _phase = crate::write_phase::enter(&"open graph (WAL replay)", 0);
            Database::new(path, SystemConfig::default())
                .map_err(|e| anyhow::anyhow!("failed to open kuzu db: {e}"))?
        };
        refuse_newer_schema(&db, path)?;
        let store = Self { db, lock_path };
        let lock = WriteLock::acquire_with_timeout(&store.lock_path, timeout)?;
        store.init_schema(&lock)?;
        // #146: a hard-exit marker only ever explains a dead-holder WAL, and
        // this function refuses one above -- so reaching here proves the state
        // the marker described is over. Drop it rather than let it outlive
        // what it explains and mislabel some later, unrelated death.
        if let Some(dir) = path.parent() {
            crate::recovery::clear_unclean_exit(dir);
        }
        drop(lock);
        Ok(store)
    }

    /// Directory containing the graph database files. Used for disk-space
    /// preflight checks before a large write (see `store_util::check_disk_headroom`)
    /// -- Kuzu aborts the whole process with an uncaught C++ exception on
    /// ENOSPC mid-transaction rather than surfacing a Rust `Result`, so
    /// callers doing a large bulk write must check headroom themselves
    /// first (observed on sittir: SCIP enrichment ran the volume out of
    /// space mid-COPY and crashed the process).
    pub fn db_dir(&self) -> Option<&Path> {
        self.lock_path.parent()
    }

    /// Open an existing Kuzu database in read-only mode.
    /// Safe for concurrent access while a watcher is writing.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        validate_db_file(path)?;
        // `validate_db_file` treats a missing file as fine ("fresh create"),
        // which is correct for the write path (`open`) but not here: a
        // read-only Kuzu connection can never create a database, so letting
        // this fall through to `Database::new` below produces Kuzu's own
        // confusing "Cannot create an empty database under READ ONLY mode"
        // instead of a clear, actionable message.
        if !path.exists() {
            anyhow::bail!(
                "no graph exists yet at {} -- run `infigraph index` first",
                path.display()
            );
        }
        let lock_path = db_lock_path(path);
        if let Some(pid) = unclean_shutdown_wal_holder(path, &lock_path) {
            // #146: if that death was a deliberate hard exit we recorded, say
            // so. The rebuild is required either way -- the WAL is still
            // unreplayable -- but "the daemon shut itself down mid-import" is
            // a very different thing to read than "your graph may be corrupt".
            let explanation = hard_exit_explanation(path, pid);
            return Err(anyhow::Error::new(GraphCorruption {
                detail: format!(
                    "graph has an unreplayed WAL from process {pid}, which is no longer \
                     running (unclean shutdown){explanation} -- refusing to open it directly \
                     since WAL replay in this state has crashed the whole process before (see \
                     github.com/pradeepmouli/infigraph#92); run `infigraph index --full` to \
                     rebuild"
                ),
            }));
        }
        // `throw_on_wal_replay_failure` defaults to true (unset here): a WAL
        // replay failure now surfaces as an error instead of being silently
        // tolerated and served as a torn base image.
        let config = SystemConfig::default()
            .read_only(true)
            .buffer_pool_size(READ_ONLY_BUFFER_POOL_BYTES);
        let db = Database::new(path, config).map_err(|e| {
            classify_read_only_open_failure(
                format!("failed to open kuzu db (read-only): {e}"),
                live_graph_writer(path, &lock_path),
            )
        })?;
        refuse_newer_schema(&db, path)?;
        Ok(Self { db, lock_path })
    }

    /// Like [`open_read_only`](Self::open_read_only), but on a dead-holder
    /// WAL, quarantines immediately and degrades to the most recent
    /// `graph.previous.<ts>` entry (read-only) instead of failing outright
    /// -- see R3.1.4b. Never triggers a write itself; it only quarantines (a
    /// data-safety move, not a write to the live graph) and leaves a
    /// sentinel for the daemon coordinator (`recovery::drain_recovery_sentinel`)
    /// to act on asynchronously.
    ///
    /// Internal/test call sites that want the strict, non-degrading
    /// behavior keep calling `open_read_only` directly -- it is unchanged.
    pub fn open_read_only_or_degrade(path: &Path) -> Result<(Self, Option<DegradeReason>)> {
        let infigraph_dir = path.parent().ok_or_else(|| {
            anyhow::anyhow!("graph path {} has no parent directory", path.display())
        })?;
        let graph_name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("graph path {} has no file name", path.display()))?
            .to_string_lossy()
            .into_owned();

        // A tripped crash-loop breaker takes precedence over everything
        // else -- a distinct, unambiguous refusal rather than another
        // quarantine attempt or degrade lookup.
        if let Some(attempts) = crate::recovery::crash_loop_detected(infigraph_dir) {
            anyhow::bail!(
                "crash-loop detected: {} auto-rebuild attempts within the last hour -- refusing \
                 further automatic rebuilds. Investigate the underlying cause, then delete {} to \
                 reset and retry manually with `infigraph index --full`.",
                attempts.len(),
                crate::recovery::crash_loop_marker_path(infigraph_dir).display(),
            );
        }

        if path.exists() {
            let lock_path = db_lock_path(path);
            if let Some(pid) = unclean_shutdown_wal_holder(path, &lock_path) {
                // `quarantine_graph` requires the caller already hold
                // `graph.lock` (same path as `lock_path` -- see
                // `db_lock_path`'s doc comment): without it, a concurrent
                // writer could replace this dead-holder graph with a
                // healthy rebuild between the peek above and the
                // quarantine call below, and this reader would then rename
                // that healthy rebuild aside as corrupt (adversarial
                // review finding on R3.1.4).
                let graph_lock = crate::lockfile::acquire(
                    &lock_path,
                    "read-triggered-quarantine",
                    QUARANTINE_LOCK_TIMEOUT,
                )?;
                // Acquiring the lock stamps our own identity into its
                // payload, so the *holder* it now names is us -- re-check
                // the on-disk condition directly instead. With the lock
                // held, no other writer can be mid-rebuild, so a WAL
                // sibling still present means the state genuinely wasn't
                // fixed while this reader waited.
                let still_dead = !wal_family_paths(path).is_empty();
                if !still_dead {
                    drop(graph_lock);
                    return Self::open_read_only(path).map(|s| (s, None));
                }
                crate::quarantine::quarantine_graph(infigraph_dir, &graph_name)?;
                crate::recovery::mark_recovery_needed(infigraph_dir, pid, path)?;
                drop(graph_lock);
                return Self::degrade_or_refuse(infigraph_dir, &graph_name, pid);
            }
            return Self::open_read_only(path).map(|s| (s, None));
        }

        // Missing path: either genuinely never indexed, or quarantined by
        // an earlier call (this reader or another one) and not yet rebuilt.
        if crate::recovery::pending_recovery(infigraph_dir) {
            // dead_pid isn't recoverable here (the sentinel only round-trips
            // it internally) -- 0 is an acceptable placeholder in the
            // returned reason since callers only use it for banner wording,
            // not logic.
            return Self::degrade_or_refuse(infigraph_dir, &graph_name, 0);
        }

        anyhow::bail!(
            "no graph exists yet at {} -- run `infigraph index` first",
            path.display()
        );
    }

    fn degrade_or_refuse(
        infigraph_dir: &Path,
        graph_name: &str,
        dead_pid: u32,
    ) -> Result<(Self, Option<DegradeReason>)> {
        if let Some(previous) =
            crate::recovery::find_most_recent_previous(infigraph_dir, graph_name)
        {
            let store = Self::open_read_only(&previous)?;
            return Ok((
                store,
                Some(DegradeReason::PreCrashSnapshot {
                    snapshot_path: previous,
                    dead_pid,
                }),
            ));
        }
        anyhow::bail!(
            "graph for {graph_name} is being automatically rebuilt after a detected crash -- \
             retry shortly"
        );
    }

    /// Acquire exclusive write lock. Waits up to 30s, returning `Busy` if
    /// still held at expiry.
    pub fn write_lock(&self) -> Result<WriteLock> {
        WriteLock::acquire(&self.lock_path)
    }

    /// Acquire the write lock with a caller-chosen wait budget.
    pub fn write_lock_with_timeout(&self, timeout: std::time::Duration) -> Result<WriteLock> {
        WriteLock::acquire_with_timeout(&self.lock_path, timeout)
    }

    /// Try to acquire write lock without blocking. Returns None if already held.
    pub fn try_write_lock(&self) -> Result<Option<WriteLock>> {
        WriteLock::try_acquire(&self.lock_path)
    }

    fn init_schema(&self, _witness: &WriteLock) -> Result<()> {
        let conn = self.connection()?;
        for ddl in CREATE_SCHEMA {
            conn.query(ddl)
                .map_err(|e| anyhow::anyhow!("schema error: {e}\n  DDL: {ddl}"))?;
        }
        for migration in MIGRATIONS {
            let _ = conn.query(migration);
        }
        // R8.1 (#85): stamp the schema version this binary just ensured.
        // ON CREATE initializes BOTH generation counters explicitly --
        // Kuzu's NULL+1=NULL arithmetic on a never-initialized column bit
        // the R3.3.4 bump helper before shipping (same lesson applied).
        conn.query(&format!(
            "MERGE (g:GraphMeta {{id: 'singleton'}}) \
             ON CREATE SET g.ast_generation = 0, g.scip_generation = 0, \
                           g.schema_version = {v} \
             ON MATCH SET g.schema_version = {v}",
            v = super::schema::SCHEMA_VERSION
        ))
        .map_err(|e| anyhow::anyhow!("failed to stamp schema version: {e}"))?;
        Ok(())
    }

    /// Bump the graph's AST generation counter (R3.3.3/docs/DESIGN-hardening.md
    /// §3.3.3) by 1, creating it at 1 on the very first write, and return
    /// the new value. Every real AST write path calls this once its write
    /// succeeds (full reindex, incremental, and watcher batches alike), so a
    /// sidecar that records the generation it was built from can detect
    /// drift against the live graph.
    ///
    /// Takes an already-open `conn` and the caller's `WriteLock` witness --
    /// mirrors the `_conn` convention used by `upsert_file_conn` etc., since
    /// every call site already holds both by the time its write completes.
    pub fn bump_ast_generation_conn(
        &self,
        conn: &Connection<'_>,
        _witness: &WriteLock,
    ) -> Result<i64> {
        bump_ast_generation(conn)
    }

    /// Stamp the graph's SCIP-enrichment generation
    /// (R3.3.4/docs/DESIGN-hardening.md §3.3.4) to the *current*
    /// `ast_generation` and return it. Only `scip::import_scip_index` calls
    /// this -- the watcher's incremental reindex never does -- so
    /// `ast_generation - scip_generation` is "AST writes since the last
    /// enrichment", which is what doctor reports and what the daemon's
    /// R3.3.4a auto-re-enrichment compares against its threshold.
    ///
    /// A stamp, not a `+1` bump: an import counter can never catch up with
    /// the AST counter (an enrichment after 700 writes would still read as
    /// 699 behind), so the gap would be meaningless and the daemon would
    /// re-enrich after every write burst. Caught on this repo's own graph
    /// the first time R3.3.4a fired for real.
    ///
    /// `enriched_ast_generation` is the generation the enrichment *started*
    /// from, when the caller knows it (the daemon captures it before it
    /// launches the indexers): the indexers run unlocked for minutes, and
    /// every drain that lands meanwhile is not in the `.scip` data, so
    /// stamping "now" would hide exactly that drift. `None` (the CLI and
    /// MCP import paths, which run the indexers right after their own scan
    /// with no concurrent writer) stamps the current generation. The value
    /// is capped at the current `ast_generation` (a full reindex can reset
    /// the counters underneath a captured value) and never moves backwards.
    ///
    /// Lands at 1 (never 0) on a graph with no AST writes yet:
    /// `scip_generation == 0` is the "SCIP never ran" sentinel everywhere.
    pub fn stamp_scip_generation_conn(
        &self,
        conn: &Connection<'_>,
        _witness: &WriteLock,
        enriched_ast_generation: Option<i64>,
    ) -> Result<i64> {
        stamp_scip_generation(conn, enriched_ast_generation)
    }

    /// Read the graph's current AST generation without bumping it. Returns 0
    /// for a graph that has never had a write bump the counter (e.g. a
    /// freshly opened, never-indexed database) -- callers comparing a
    /// sidecar's recorded generation against this should treat 0 as "no
    /// generation recorded yet", not a real generation value.
    pub fn current_ast_generation(&self) -> Result<i64> {
        read_generation_field(self, "ast_generation")
    }

    /// Read the graph's current SCIP-enrichment generation without bumping
    /// it. Returns 0 both for "never indexed at all" and for "indexed, but
    /// SCIP enrichment has never run" (e.g. no applicable SCIP indexer for
    /// the project's languages) -- callers must not treat a bare 0 as
    /// staleness on its own; see `doctor::check_scip_staleness` for the
    /// actual "meaningfully behind" comparison.
    pub fn current_scip_generation(&self) -> Result<i64> {
        read_generation_field(self, "scip_generation")
    }

    pub fn connection(&self) -> Result<Connection<'_>> {
        Connection::new(&self.db).map_err(|e| anyhow::anyhow!("failed to create connection: {e}"))
    }

    /// Run `f` inside a real transaction: opens one connection, issues
    /// `BEGIN TRANSACTION`, runs `f` against that connection, commits on
    /// `Ok`, rolls back on `Err`.
    ///
    /// Use this for any write that must be atomic across more than one
    /// statement. `KuzuBackend::raw_query`'s `BEGIN`/`COMMIT`/`ROLLBACK`
    /// handling is deliberately a no-op (each call opens a fresh connection,
    /// so transaction-control statements issued through it can never span
    /// more than the one statement they're attached to) -- passing those
    /// strings to `raw_query` does not get you a transaction. This method is
    /// the real thing.
    pub fn transaction<T>(&self, f: impl FnOnce(&Connection<'_>) -> Result<T>) -> Result<T> {
        let conn = self.connection()?;
        conn.query("BEGIN TRANSACTION")
            .map_err(|e| anyhow::anyhow!("failed to begin transaction: {e}"))?;
        match f(&conn) {
            Ok(value) => {
                // COMMIT is where lbug runs its forced/auto checkpoint --
                // the step most worth naming if the process aborts (#132).
                let _phase = crate::write_phase::enter(&"transaction: COMMIT", 0);
                conn.query("COMMIT")
                    .map_err(|e| anyhow::anyhow!("failed to commit transaction: {e}"))?;
                Ok(value)
            }
            Err(e) => {
                let _phase = crate::write_phase::enter(&"transaction: ROLLBACK", 0);
                let _ = conn.query("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Remove all graph data for a deleted file.
    pub fn remove_file(&self, file: &str) -> Result<()> {
        let lock = self.write_lock()?;
        let conn = self.connection()?;
        self.remove_file_conn(&conn, file, &lock)
    }

    pub fn remove_file_conn(
        &self,
        conn: &Connection<'_>,
        file: &str,
        _witness: &WriteLock,
    ) -> Result<()> {
        let _phase = crate::write_phase::enter(&"remove-file", 1);
        let _ = conn.query(&format!(
            "MATCH (f:File)-[:DEFINES]->(s:Symbol)-[:HAS_STATEMENT]->(st:Statement) WHERE f.id = '{}' DETACH DELETE st",
            escape(file)
        ));
        let _ = conn.query(&format!(
            "MATCH (s:Symbol) WHERE s.file = '{}' DETACH DELETE s",
            escape(file)
        ));
        let _ = conn.query(&format!(
            "MATCH (m:Module) WHERE m.file = '{}' DETACH DELETE m",
            escape(file)
        ));
        let _ = conn.query(&format!(
            "MATCH (f:File) WHERE f.id = '{}' DETACH DELETE f",
            escape(file)
        ));
        Ok(())
    }

    /// Remove all files whose path starts with the given prefix (handles directory removal).
    pub fn remove_files_by_prefix(&self, prefix: &str) -> Result<usize> {
        let lock = self.write_lock()?;
        let conn = self.connection()?;
        let escaped = escape(prefix);
        let result = conn
            .query(&format!(
                "MATCH (f:File) WHERE f.id STARTS WITH '{escaped}' RETURN f.id"
            ))
            .map_err(|e| anyhow::anyhow!("query files by prefix: {e}"))?;
        let mut files = Vec::new();
        for row in result {
            if let Some(val) = row.first() {
                files.push(val.to_string());
            }
        }
        for f in &files {
            self.remove_file_conn(&conn, f, &lock)?;
        }
        Ok(files.len())
    }

    /// Return map of file path -> content_hash for all indexed modules.
    /// Used by incremental indexing to skip unchanged files.
    pub fn get_file_hashes(&self) -> Result<HashMap<String, String>> {
        let conn = self.connection()?;
        let result = conn
            .query("MATCH (m:Module) RETURN m.file, m.content_hash")
            .map_err(|e| anyhow::anyhow!("get_file_hashes failed: {e}"))?;
        let mut map = HashMap::new();
        for row in result {
            if row.len() >= 2 {
                map.insert(row[0].to_string(), row[1].to_string());
            }
        }
        Ok(map)
    }

    /// Return all symbols as (name, id, file, kind) tuples -- used by resolve_calls.
    pub fn get_all_symbols(&self) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.connection()?;
        let result = conn
            .query("MATCH (s:Symbol) RETURN s.name, s.id, s.file, s.kind")
            .map_err(|e| anyhow::anyhow!("get_all_symbols failed: {e}"))?;
        let mut symbols = Vec::new();
        for row in result {
            if row.len() >= 4 {
                symbols.push((
                    row[0].to_string(),
                    row[1].to_string(),
                    row[2].to_string(),
                    row[3].to_string(),
                ));
            }
        }
        Ok(symbols)
    }

    /// Get total counts for stats.
    pub fn derive_tested_by_edges(&self) -> Result<usize> {
        let _lock = self.write_lock()?;
        let conn = self.connection()?;
        let q = super::queries::GraphQuery::new(&conn);
        q.derive_tested_by_edges()
    }

    pub fn stats(&self) -> Result<GraphStats> {
        let conn = self.connection()?;

        let symbol_count = count_query(&conn, "MATCH (s:Symbol) RETURN count(s)")?;
        let module_count = count_query(&conn, "MATCH (m:Module) RETURN count(m)")?;
        let file_count = count_query(&conn, "MATCH (f:File) RETURN count(f)")?;
        let folder_count = count_query(&conn, "MATCH (d:Folder) RETURN count(d)")?;
        let calls_count = count_query(&conn, "MATCH ()-[r:CALLS]->() RETURN count(r)")?;
        let inherits_count = count_query(&conn, "MATCH ()-[r:INHERITS]->() RETURN count(r)")?;
        let contains_count = count_query(&conn, "MATCH ()-[r:CONTAINS]->() RETURN count(r)")?;

        Ok(GraphStats {
            symbols: symbol_count,
            modules: module_count,
            files: file_count,
            folders: folder_count,
            calls: calls_count,
            inherits: inherits_count,
            contains: contains_count,
        })
    }
}

#[derive(Debug)]
pub struct GraphStats {
    pub symbols: u64,
    pub modules: u64,
    pub files: u64,
    pub folders: u64,
    pub calls: u64,
    pub inherits: u64,
    pub contains: u64,
}

impl std::fmt::Display for GraphStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Graph Statistics:")?;
        writeln!(f, "  Symbols:      {}", self.symbols)?;
        writeln!(f, "  Modules:      {}", self.modules)?;
        writeln!(f, "  Files:        {}", self.files)?;
        writeln!(f, "  Folders:      {}", self.folders)?;
        writeln!(f, "  Calls edges:  {}", self.calls)?;
        writeln!(f, "  Inherits:     {}", self.inherits)?;
        writeln!(f, "  Contains:     {}", self.contains)
    }
}

fn count_query(conn: &Connection, query: &str) -> Result<u64> {
    let mut result = conn
        .query(query)
        .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
    if let Some(row) = result.next() {
        if let Some(val) = row.first() {
            return Ok(val.to_string().parse().unwrap_or(0));
        }
    }
    Ok(0)
}

/// Implementation behind `bump_ast_generation_conn`: `ast_generation + 1`,
/// creating the singleton row at 1 if it doesn't exist yet.
///
/// `ON CREATE` must initialize *both* GraphMeta columns: the singleton row
/// is shared with `scip_generation`, so if it doesn't exist yet this could
/// be the first write of either one. Setting only one field would leave
/// the other NULL, and a later `+1` on it would compute `NULL + 1` = NULL
/// (caught by the predecessor of
/// `scip_generation_starts_at_zero_and_stamps_to_the_ast_generation_it_enriched`
/// before this shipped: two ast bumps created the row with only
/// ast_generation set, then the scip write found scip_generation NULL).
/// `stamp_scip_generation` below initializes both for the same reason.
fn bump_ast_generation(conn: &Connection) -> Result<i64> {
    write_generation_field(
        conn,
        "ast_generation",
        "ON CREATE SET g.ast_generation = 1, g.scip_generation = 0 \
         ON MATCH SET g.ast_generation = g.ast_generation + 1",
    )
}

/// Implementation behind `stamp_scip_generation_conn` -- see its doc
/// comment for the semantics. Computed here rather than in Cypher so the
/// three clamps (cap at `ast_generation`, never backwards, floor 1) read
/// as one expression.
fn stamp_scip_generation(conn: &Connection, enriched_ast_generation: Option<i64>) -> Result<i64> {
    let ast = read_generation_field_conn(conn, "ast_generation")?;
    let scip = read_generation_field_conn(conn, "scip_generation")?;
    let target = enriched_ast_generation
        .unwrap_or(ast)
        .min(ast)
        .max(scip)
        .max(1);
    write_generation_field(
        conn,
        "scip_generation",
        &format!(
            "ON CREATE SET g.ast_generation = 0, g.scip_generation = {target} \
             ON MATCH SET g.scip_generation = {target}"
        ),
    )
}

/// Runs `MERGE (g:GraphMeta {id: 'singleton'}) <set_clauses>` and reads
/// back `g.<field>`. `field` is always a literal from the two callers
/// above, never external input, so interpolating it is safe.
fn write_generation_field(
    conn: &Connection,
    field: &'static str,
    set_clauses: &str,
) -> Result<i64> {
    conn.query(&format!(
        "MERGE (g:GraphMeta {{id: 'singleton'}}) {set_clauses}"
    ))
    .map_err(|e| anyhow::anyhow!("failed to write graph {field}: {e}"))?;
    let mut result = conn
        .query(&format!(
            "MATCH (g:GraphMeta {{id: 'singleton'}}) RETURN g.{field}"
        ))
        .map_err(|e| anyhow::anyhow!("failed to read graph {field} after write: {e}"))?;
    let row = result
        .next()
        .context("GraphMeta singleton missing immediately after MERGE")?;
    let val = row
        .first()
        .with_context(|| format!("graph {field} query returned an empty row"))?;
    val.to_string()
        .parse()
        .with_context(|| format!("graph {field} value is not a valid integer"))
}

/// Shared implementation behind `current_ast_generation`/
/// `current_scip_generation`: read `GraphMeta.<field>` without bumping it.
/// Returns 0 (see both callers' doc comments for what 0 means for each
/// field) when the singleton row doesn't exist or the field can't be
/// parsed.
fn read_generation_field(store: &GraphStore, field: &'static str) -> Result<i64> {
    let conn = store.connection()?;
    read_generation_field_conn(&conn, field)
}

/// `read_generation_field` on an already-open connection (the stamp path
/// reads both counters on the write connection it is about to write with).
fn read_generation_field_conn(conn: &Connection, field: &'static str) -> Result<i64> {
    Ok(count_query(
        conn,
        &format!("MATCH (g:GraphMeta {{id: 'singleton'}}) RETURN g.{field}"),
    )
    .unwrap_or(0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_holder_lock(lock_path: &Path, pid: u32) {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let info = lockfile::LockInfo {
            pid,
            role: "test".to_string(),
            build_hash: "test".to_string(),
            acquired_at: 0,
            last_heartbeat: 0,
            holder_started_at: 0,
        };
        std::fs::write(lock_path, serde_json::to_string(&info).unwrap()).unwrap();
    }

    /// A PID essentially guaranteed not to be a running process, standing
    /// in for "the write lock's recorded holder is dead" across these tests.
    const DEAD_PID: u32 = 999_999;

    #[test]
    fn unclean_shutdown_wal_holder_flags_wal_plus_dead_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        std::fs::write(dir.path().join("graph.wal"), b"wal").unwrap();
        let lock_path = db_lock_path(&db_path);
        write_holder_lock(&lock_path, DEAD_PID);

        assert_eq!(
            unclean_shutdown_wal_holder(&db_path, &lock_path),
            Some(DEAD_PID)
        );
    }

    #[test]
    fn unclean_shutdown_wal_holder_ignores_a_live_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        std::fs::write(dir.path().join("graph.wal"), b"wal").unwrap();
        let lock_path = db_lock_path(&db_path);
        write_holder_lock(&lock_path, std::process::id());

        assert_eq!(
            unclean_shutdown_wal_holder(&db_path, &lock_path),
            None,
            "a live writer's WAL is routine, not a signal to refuse opening"
        );
    }

    #[test]
    fn unclean_shutdown_wal_holder_ignores_no_wal_even_with_a_dead_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        let lock_path = db_lock_path(&db_path);
        write_holder_lock(&lock_path, DEAD_PID);

        assert_eq!(
            unclean_shutdown_wal_holder(&db_path, &lock_path),
            None,
            "a dead holder alone (no WAL) means nothing was left mid-replay"
        );
    }

    #[test]
    fn unclean_shutdown_wal_holder_ignores_a_wal_with_no_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        std::fs::write(dir.path().join("graph.wal"), b"wal").unwrap();
        let lock_path = db_lock_path(&db_path);

        assert_eq!(
            unclean_shutdown_wal_holder(&db_path, &lock_path),
            None,
            "can't confirm a dead holder without a lock payload to read -- conservative by design"
        );
    }

    /// Regression test for github.com/pradeepmouli/infigraph#92: a stale WAL
    /// from a dead process used to be handed straight to `kuzu::Database::new`,
    /// which crashed the whole process with SIGBUS deep inside WAL replay --
    /// before any `Result` existed to catch it. Both `open` and
    /// `open_read_only` must refuse up front instead.
    #[test]
    fn open_refuses_a_graph_with_an_unreplayed_wal_from_a_dead_process() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        // Large enough to pass `validate_db_file`'s truncation check -- this
        // test is about the WAL-replay guard, not that one.
        std::fs::write(&db_path, vec![0u8; MIN_DB_FILE_SIZE as usize]).unwrap();
        std::fs::write(dir.path().join("graph.wal"), b"wal").unwrap();
        write_holder_lock(&db_lock_path(&db_path), DEAD_PID);

        let err = GraphStore::open(&db_path)
            .map(|_| ())
            .expect_err("must refuse rather than attempt Database::new");
        assert!(err.to_string().contains("unreplayed WAL"), "{err}");
        assert!(err.to_string().contains(&DEAD_PID.to_string()), "{err}");

        let err = GraphStore::open_read_only(&db_path)
            .map(|_| ())
            .expect_err("read-only open must refuse too");
        assert!(err.to_string().contains("unreplayed WAL"), "{err}");
        assert!(
            err.downcast_ref::<GraphCorruption>().is_some(),
            "read-only path's error must downcast to GraphCorruption so callers that route to \
             quarantine (R3.1.2) can catch it: {err}"
        );
    }

    #[test]
    fn open_read_only_or_degrade_falls_back_to_the_previous_pool() {
        let dir = tempfile::tempdir().unwrap();
        let infigraph_dir = dir.path();
        let db_path = infigraph_dir.join("graph");

        // Seed a real, openable "previous" graph a full reindex would have
        // retired -- open+init it via the normal write path, then rename it
        // aside exactly as `quarantine::retire_previous_graph` would.
        GraphStore::open(&db_path).unwrap();
        let previous_path = infigraph_dir.join("graph.previous.111");
        std::fs::rename(&db_path, &previous_path).unwrap();

        // Recreate the live path as a dead-holder-WAL scenario.
        std::fs::write(&db_path, vec![0u8; MIN_DB_FILE_SIZE as usize]).unwrap();
        std::fs::write(infigraph_dir.join("graph.wal"), b"stub wal").unwrap();
        write_holder_lock(&db_lock_path(&db_path), DEAD_PID);

        let (store, reason) = GraphStore::open_read_only_or_degrade(&db_path).unwrap();
        drop(store);

        match reason {
            Some(DegradeReason::PreCrashSnapshot { snapshot_path, .. }) => {
                assert_eq!(snapshot_path, previous_path);
            }
            other => panic!("expected PreCrashSnapshot degrade, got {other:?}"),
        }
        assert!(
            crate::recovery::pending_recovery(infigraph_dir),
            "sentinel must be left for the daemon coordinator to pick up"
        );
        assert!(
            !db_path.exists(),
            "the dead-holder graph must have been quarantined"
        );
    }

    #[test]
    fn open_read_only_or_degrade_refuses_with_rebuild_in_progress_wording_when_no_fallback_exists()
    {
        let dir = tempfile::tempdir().unwrap();
        let infigraph_dir = dir.path();
        let db_path = infigraph_dir.join("graph");

        std::fs::write(&db_path, vec![0u8; MIN_DB_FILE_SIZE as usize]).unwrap();
        std::fs::write(infigraph_dir.join("graph.wal"), b"stub wal").unwrap();
        write_holder_lock(&db_lock_path(&db_path), DEAD_PID);

        let err = GraphStore::open_read_only_or_degrade(&db_path)
            .map(|_| ())
            .expect_err("no .previous. pool entry exists -- must refuse");
        assert!(
            err.to_string().contains("automatically rebuilt"),
            "must say a rebuild is already in progress, not tell the human to run --full: {err}"
        );
    }

    #[test]
    fn open_read_only_or_degrade_refuses_distinctly_once_the_crash_loop_breaker_has_tripped() {
        let dir = tempfile::tempdir().unwrap();
        let infigraph_dir = dir.path();
        std::fs::create_dir_all(infigraph_dir).unwrap();
        crate::recovery::write_crash_loop_marker(infigraph_dir, &[1, 2]).unwrap();

        let db_path = infigraph_dir.join("graph"); // doesn't need to exist for this path
        let err = GraphStore::open_read_only_or_degrade(&db_path)
            .map(|_| ())
            .expect_err("crash-loop marker must short-circuit to a refusal");
        assert!(
            err.to_string().contains("crash-loop"),
            "must be the distinct crash-loop wording, not the generic quarantine message: {err}"
        );
    }

    /// Regression test (adversarial review of R3.1.4): `open_read_only_or_degrade`
    /// used to call `quarantine_graph` without holding `graph.lock`, despite
    /// `quarantine_graph`'s explicit caller-holds-the-lock contract. A reader
    /// that peeked a dead-holder WAL could still be racing a concurrent writer
    /// that legitimately rebuilds the graph in the meantime -- destroying a
    /// completed rebuild by quarantining it as if it were still corrupt.
    #[test]
    fn open_read_only_or_degrade_does_not_quarantine_a_graph_a_concurrent_writer_just_healed() {
        let dir = tempfile::tempdir().unwrap();
        let infigraph_dir = dir.path();
        let db_path = infigraph_dir.join("graph");

        // Seed the dead-holder-WAL scenario the initial (pre-lock) peek must see.
        std::fs::write(&db_path, vec![0u8; MIN_DB_FILE_SIZE as usize]).unwrap();
        std::fs::write(infigraph_dir.join("graph.wal"), b"stub wal").unwrap();
        let lock_path = db_lock_path(&db_path);
        write_holder_lock(&lock_path, DEAD_PID);

        // Hold the raw OS flock ourselves -- deliberately bypassing
        // `lockfile::acquire`'s identity stamp, which would overwrite the
        // dead-pid payload the pre-lock peek below needs to still see.
        // This forces the reader to block on acquisition exactly like a
        // real concurrent writer would.
        let held_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&held_file).unwrap();

        let infigraph_dir_owned = infigraph_dir.to_path_buf();
        let db_path_for_writer = db_path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            // Simulate a completed concurrent rebuild while still holding
            // the raw flock, exactly as a real writer holds `graph.lock`
            // for its whole rebuild: replace the stub with a real,
            // cleanly-closed graph (no WAL sibling left behind), all
            // before releasing. Using `GraphStore::open` here would
            // self-deadlock -- it acquires this same lock internally.
            std::fs::remove_file(&db_path_for_writer).unwrap();
            std::fs::remove_file(infigraph_dir_owned.join("graph.wal")).unwrap();
            Database::new(&db_path_for_writer, SystemConfig::default()).unwrap();
            fs2::FileExt::unlock(&held_file).unwrap();
        });

        let (store, reason) = GraphStore::open_read_only_or_degrade(&db_path).unwrap();
        writer.join().unwrap();
        drop(store);

        assert!(
            reason.is_none(),
            "graph was healed by the time the lock was acquired -- must open it normally, not degrade: {reason:?}"
        );
        assert!(
            db_path.exists(),
            "the concurrently-rebuilt graph must not have been quarantined"
        );
        assert!(
            !crate::recovery::pending_recovery(infigraph_dir),
            "no recovery sentinel should be left behind for a graph that was never actually quarantined"
        );
    }

    /// Regression test: a truncated graph file used to be handed straight to
    /// `kuzu::Database::new`, which parses a bogus size field from the header
    /// and either aborts the process (Linux) or segfaults later at read time
    /// (macOS, `BufferManager::optimisticRead`). The preflight must turn this
    /// into a normal `Err` so `Infigraph::init`'s wipe-and-rebuild path runs.
    #[test]
    fn open_truncated_db_file_returns_err_instead_of_aborting() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        std::fs::write(&db_path, b"garbage, way below one page").unwrap();

        let err = GraphStore::open(&db_path)
            .map(|_| ())
            .expect_err("truncated file must be rejected");
        assert!(
            err.to_string().contains("truncated/corrupt"),
            "unexpected error: {err}"
        );

        let err = GraphStore::open_read_only(&db_path)
            .map(|_| ())
            .expect_err("truncated file must be rejected in read-only mode too");
        assert!(err.to_string().contains("truncated/corrupt"));
    }

    #[test]
    fn validate_db_file_accepts_missing_path_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Missing → fresh create, fine.
        assert!(validate_db_file(&dir.path().join("does-not-exist")).is_ok());
        // Directory (legacy layout) → fine.
        assert!(validate_db_file(dir.path()).is_ok());
    }

    #[test]
    fn schema_version_is_stamped_on_open_and_a_newer_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");

        // Fresh open stamps the current version; reopen proceeds fine.
        drop(GraphStore::open(&db_path).unwrap());
        let store = GraphStore::open(&db_path).unwrap();
        let conn = store.connection().unwrap();
        let mut result = conn
            .query("MATCH (g:GraphMeta {id: 'singleton'}) RETURN g.schema_version")
            .unwrap();
        let stamped: i64 = result
            .next()
            .and_then(|row| row.first().and_then(|v| v.to_string().parse().ok()))
            .unwrap();
        assert_eq!(stamped, super::super::schema::SCHEMA_VERSION);

        // Simulate a database written by a FUTURE binary.
        conn.query(&format!(
            "MATCH (g:GraphMeta {{id: 'singleton'}}) SET g.schema_version = {}",
            super::super::schema::SCHEMA_VERSION + 1
        ))
        .unwrap();
        drop(result);
        drop(conn);
        drop(store);

        let err = GraphStore::open(&db_path)
            .map(|_| ())
            .expect_err("a newer schema must be refused, not open-and-guessed");
        assert!(err.to_string().contains("newer infigraph"), "{err}");
        let err = GraphStore::open_read_only(&db_path)
            .map(|_| ())
            .expect_err("read-only must refuse a newer schema too");
        assert!(err.to_string().contains("newer infigraph"), "{err}");
    }

    #[test]
    fn unstamped_or_older_schema_is_migrated_forward_and_restamped() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        {
            let store = GraphStore::open(&db_path).unwrap();
            let conn = store.connection().unwrap();
            // Roll the stamp back to "pre-versioning" (0).
            conn.query("MATCH (g:GraphMeta {id: 'singleton'}) SET g.schema_version = 0")
                .unwrap();
        }
        let store = GraphStore::open(&db_path).expect("older/unstamped must open and migrate");
        let conn = store.connection().unwrap();
        let mut result = conn
            .query("MATCH (g:GraphMeta {id: 'singleton'}) RETURN g.schema_version")
            .unwrap();
        let stamped: i64 = result
            .next()
            .and_then(|row| row.first().and_then(|v| v.to_string().parse().ok()))
            .unwrap();
        assert_eq!(
            stamped,
            super::super::schema::SCHEMA_VERSION,
            "open must re-stamp after migrating forward"
        );
    }

    #[test]
    fn wal_family_covers_the_appended_wal_and_its_temp_siblings() {
        let dir = tempfile::tempdir().unwrap();
        // An extensioned db name is the case `with_extension("wal")` gets
        // wrong ("docs.wal" instead of "docs.kuzu.wal"), so use it here.
        let db_path = dir.path().join("docs.kuzu");
        std::fs::write(&db_path, b"base image").unwrap();
        std::fs::write(dir.path().join("docs.kuzu.wal"), b"wal").unwrap();
        std::fs::write(dir.path().join("docs.kuzu.wal.checkpoint"), b"ckpt").unwrap();
        // Neither a different db's family nor the base image itself.
        std::fs::write(dir.path().join("graph.wal"), b"other db").unwrap();

        let mut found: Vec<String> = wal_family_paths(&db_path)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        found.sort();
        assert_eq!(found, vec!["docs.kuzu.wal", "docs.kuzu.wal.checkpoint"]);

        remove_wal_family(&db_path);
        assert!(!dir.path().join("docs.kuzu.wal").exists());
        assert!(!dir.path().join("docs.kuzu.wal.checkpoint").exists());
        assert!(db_path.exists(), "the base image must be left alone");
        assert!(
            dir.path().join("graph.wal").exists(),
            "another database's WAL must be left alone"
        );
    }

    /// #137: the five `write_lock*.rs` tests computed their expected lock
    /// path with `db_path.with_extension("lock")`, which REPLACES an
    /// extension -- so on their "x.db" fixtures they watched "x.lock" while
    /// the store locked "x.db.lock" and never saw the lock as held. Pin the
    /// append-not-replace rule so the two can't drift apart again.
    #[test]
    fn db_lock_path_appends_rather_than_replacing_the_extension() {
        assert_eq!(
            db_lock_path(Path::new("/p/.infigraph/graph")),
            PathBuf::from("/p/.infigraph/graph.lock")
        );
        assert_eq!(
            db_lock_path(Path::new("/p/.infigraph/docs.kuzu")),
            PathBuf::from("/p/.infigraph/docs.kuzu.lock")
        );
        assert_eq!(
            db_lock_path(Path::new("/p/cross_thread.db")),
            PathBuf::from("/p/cross_thread.db.lock")
        );
        assert_ne!(
            db_lock_path(Path::new("/p/graph.rebuilding")),
            db_lock_path(Path::new("/p/graph")),
            "a rebuild scratch graph must not share the live graph's lock"
        );
    }

    #[test]
    fn wal_family_of_a_db_that_never_existed_is_empty_and_removal_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph.rebuilding");
        assert!(wal_family_paths(&db_path).is_empty());
        remove_wal_family(&db_path);
    }

    #[test]
    fn open_fresh_then_reopen_still_works_with_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        // Fresh create.
        drop(GraphStore::open(&db_path).expect("fresh create must succeed"));
        // Reopen of a valid db must pass the preflight.
        drop(GraphStore::open(&db_path).expect("reopen of valid db must succeed"));
    }

    #[test]
    fn ast_generation_starts_at_zero_and_bumps_monotonically() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        let store = GraphStore::open(&db_path).unwrap();

        assert_eq!(
            store.current_ast_generation().unwrap(),
            0,
            "a never-bumped graph reports ast_generation 0"
        );

        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();

        let first = store.bump_ast_generation_conn(&conn, &lock).unwrap();
        assert_eq!(first, 1, "the first bump creates the counter at 1");
        assert_eq!(store.current_ast_generation().unwrap(), 1);

        let second = store.bump_ast_generation_conn(&conn, &lock).unwrap();
        assert_eq!(second, 2, "each subsequent bump increments by 1");
        assert_eq!(store.current_ast_generation().unwrap(), 2);
    }

    #[test]
    fn scip_generation_starts_at_zero_and_stamps_to_the_ast_generation_it_enriched() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        let store = GraphStore::open(&db_path).unwrap();

        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();

        // AST-only writes (watcher batches, ordinary reindexes) must never
        // move scip_generation -- that's the entire point of R3.3.4's split.
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        assert_eq!(store.current_ast_generation().unwrap(), 2);
        assert_eq!(
            store.current_scip_generation().unwrap(),
            0,
            "ast bumps must not advance scip_generation"
        );

        // A SCIP import records WHICH ast generation it enriched, so
        // `ast_generation - scip_generation` is "writes since the last
        // enrichment" (what doctor and the daemon's R3.3.4a trigger compare)
        // -- not a running count of imports, which could never catch up.
        let stamped = store
            .stamp_scip_generation_conn(&conn, &lock, None)
            .unwrap();
        assert_eq!(stamped, 2);
        assert_eq!(store.current_scip_generation().unwrap(), 2);
        // And the reverse: a SCIP stamp must not advance ast_generation.
        assert_eq!(store.current_ast_generation().unwrap(), 2);

        // Two more AST writes: 2 behind. Re-enrich: caught up again.
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        assert_eq!(store.current_scip_generation().unwrap(), 2);
        assert_eq!(
            store
                .stamp_scip_generation_conn(&conn, &lock, None)
                .unwrap(),
            4
        );
    }

    #[test]
    fn scip_stamp_records_the_generation_enrichment_started_from_not_import_time() {
        // The indexers run unlocked for minutes; drains that land meanwhile
        // are NOT in the .scip data. The import therefore stamps the
        // generation the enrichment started from (captured by the daemon),
        // capped at the current ast_generation and never moving backwards.
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph")).unwrap();
        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        for _ in 0..5 {
            store.bump_ast_generation_conn(&conn, &lock).unwrap();
        }

        // Started at 3, five writes exist now: 2 of them are still stale.
        assert_eq!(
            store
                .stamp_scip_generation_conn(&conn, &lock, Some(3))
                .unwrap(),
            3
        );
        // A captured value ahead of the graph (a full reindex reset the
        // counters underneath) is capped at what the graph actually has.
        assert_eq!(
            store
                .stamp_scip_generation_conn(&conn, &lock, Some(10))
                .unwrap(),
            5
        );
        // Never backwards: a late-landing older import must not un-enrich.
        assert_eq!(
            store
                .stamp_scip_generation_conn(&conn, &lock, Some(2))
                .unwrap(),
            5
        );
        // And the sentinel floor still holds for a captured 0.
        let fresh = GraphStore::open(&dir.path().join("graph2")).unwrap();
        let fresh_lock = fresh.write_lock().unwrap();
        let fresh_conn = fresh.connection().unwrap();
        assert_eq!(
            fresh
                .stamp_scip_generation_conn(&fresh_conn, &fresh_lock, Some(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn scip_stamp_on_a_graph_with_no_ast_writes_still_reads_as_enriched() {
        // scip_generation == 0 is the "SCIP never ran" sentinel everywhere
        // (doctor, the daemon's staleness trigger). A stamp taken before any
        // AST write must therefore land at 1, not copy ast_generation's 0.
        let dir = tempfile::tempdir().unwrap();
        let store = GraphStore::open(&dir.path().join("graph")).unwrap();
        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();

        assert_eq!(
            store
                .stamp_scip_generation_conn(&conn, &lock, None)
                .unwrap(),
            1
        );
        assert_eq!(store.current_scip_generation().unwrap(), 1);
        assert_eq!(store.current_ast_generation().unwrap(), 0);
    }

    /// Regression test: `open_read_only` on a nonexistent graph must fail
    /// with a clear, actionable error rather than falling through to Kuzu's
    /// own confusing "Cannot create an empty database under READ ONLY mode"
    /// (read-only mode can never create a database).
    /// The exact message lbug produced on sittir 2026-09-02 while an idle
    /// daemon held an uncheckpointed WAL. `infigraph verify` called it
    /// corruption three runs running and advised `index --full`; stopping
    /// that daemon checkpointed the WAL away and the same untouched file
    /// verified 4 PASS / 0 WARN / 0 FAIL. The graph was never damaged.
    const LIVE_WRITER_WAL_ERROR: &str =
        "failed to open kuzu db (read-only): Runtime exception: Corrupted wal file. \
         Read out invalid WAL record type.";

    #[test]
    fn a_live_writers_unreadable_wal_is_busy_not_corruption() {
        let err = classify_read_only_open_failure(LIVE_WRITER_WAL_ERROR.to_string(), Some(4242));

        assert!(
            err.downcast_ref::<GraphCorruption>().is_none(),
            "must not downcast to GraphCorruption -- that is the quarantine/rotate \
             signal, and this graph is merely busy: {err}"
        );
        assert!(
            is_transient_open_error(&err),
            "must stay retryable so open_kuzu_with_retry and verify treat it as a race: {err}"
        );
        assert!(
            err.to_string().contains("4242"),
            "must name the holder: {err}"
        );
        assert!(
            err.to_string().contains("not corruption"),
            "must say so in as many words, since the raw lbug text says 'Corrupted': {err}"
        );
    }

    /// The gap d6a742f left open, seen live on 2026-09-04.
    ///
    /// A daemon keeps the database open for its whole session but takes
    /// `graph.lock` only around write transactions, and the lockfile blanks
    /// the payload on release. Between transactions the file is open, the WAL
    /// is uncheckpointed, and the payload is empty -- so the payload alone
    /// reports "no live holder" and a healthy graph gets classified as
    /// corrupt. `lsof` showed exactly this: the daemon holding a 135MB graph
    /// with a 1.2MB WAL while `graph.lock` was zero bytes.
    #[test]
    fn an_open_file_stands_in_for_an_empty_lock_payload() {
        assert_eq!(
            live_graph_writer_from(None, &[10642]),
            Some(10642),
            "an empty payload must not outrank a process demonstrably holding the graph open"
        );
    }

    /// The payload is the precise signal, so it wins when it has one --
    /// the file probe only ever fills the gap where it does not.
    #[test]
    fn the_lock_payload_outranks_the_file_probe() {
        assert_eq!(live_graph_writer_from(Some(4242), &[10642]), Some(4242));
    }

    /// `pids_holding_file` returns empty for "unknown" (no implementation
    /// outside Linux/macOS) as well as for "nobody", so an empty list must
    /// leave the verdict exactly where it was -- this probe can add
    /// confidence that a writer exists, never prove one absent.
    #[test]
    fn no_holders_at_all_changes_nothing() {
        assert_eq!(live_graph_writer_from(None, &[]), None);
    }

    #[test]
    fn the_same_wal_failure_with_no_live_holder_is_still_corruption() {
        let err = classify_read_only_open_failure(LIVE_WRITER_WAL_ERROR.to_string(), None);
        assert!(
            err.downcast_ref::<GraphCorruption>().is_some(),
            "with nobody holding the graph there is no race to blame -- real damage must \
             still reach the quarantine path: {err}"
        );
    }

    #[test]
    fn a_non_wal_open_failure_is_never_corruption_either_way() {
        for holder in [Some(4242), None] {
            let err = classify_read_only_open_failure(
                "failed to open kuzu db (read-only): Permission denied".to_string(),
                holder,
            );
            assert!(
                err.downcast_ref::<GraphCorruption>().is_none(),
                "an unrelated open failure must not be dressed up as corruption: {err}"
            );
        }
    }

    /// #146: a deliberate hard exit mid-write manufactures exactly the
    /// dead-holder state the R3.1.3 guard refuses, and the guard cannot tell
    /// it apart from an unexplained death -- so the user is told their graph
    /// may be corrupt when in fact the daemon shut itself down. The rebuild
    /// is still required (the WAL is still unreplayable), but the refusal
    /// must say which happened.
    #[test]
    fn a_recorded_hard_exit_explains_itself_in_the_dead_holder_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        std::fs::write(&db_path, vec![0u8; MIN_DB_FILE_SIZE as usize]).unwrap();
        std::fs::write(dir.path().join("graph.wal"), b"wal").unwrap();
        write_holder_lock(&db_lock_path(&db_path), DEAD_PID);

        // Without a marker: the bare refusal, as before.
        let err = GraphStore::open_read_only(&db_path)
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("unreplayed WAL"), "{err}");
        assert!(
            !err.to_string().contains("on purpose"),
            "nothing recorded, so nothing to explain: {err}"
        );

        // With a marker naming that pid: the refusal says what happened.
        let payload = serde_json::json!({
            "pid": DEAD_PID,
            "reason": "daemon graceful shutdown exceeded its budget",
            "phase": "scip-import (x113000)",
            "at": 1_788_400_000u64,
        });
        std::fs::write(
            crate::recovery::unclean_exit_marker_path(dir.path()),
            serde_json::to_string(&payload).unwrap(),
        )
        .unwrap();

        let err = GraphStore::open_read_only(&db_path)
            .map(|_| ())
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("hard-exited on purpose"), "{msg}");
        assert!(
            msg.contains("graceful shutdown exceeded its budget"),
            "{msg}"
        );
        assert!(
            msg.contains("scip-import"),
            "must name the write in flight: {msg}"
        );
        assert!(
            msg.contains("rather than damage"),
            "must not leave the reader thinking the graph is corrupt: {msg}"
        );
        assert!(
            msg.contains("index --full"),
            "the rebuild is still required either way: {msg}"
        );
        assert!(
            err.downcast_ref::<GraphCorruption>().is_some(),
            "still routes to quarantine -- the WAL really is unreplayable"
        );
    }

    /// A marker from some *other* process must not explain away this death.
    #[test]
    fn a_hard_exit_marker_for_a_different_pid_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        std::fs::write(&db_path, vec![0u8; MIN_DB_FILE_SIZE as usize]).unwrap();
        std::fs::write(dir.path().join("graph.wal"), b"wal").unwrap();
        write_holder_lock(&db_lock_path(&db_path), DEAD_PID);
        std::fs::write(
            crate::recovery::unclean_exit_marker_path(dir.path()),
            serde_json::json!({
                "pid": DEAD_PID + 1,
                "reason": "someone else's exit",
                "phase": serde_json::Value::Null,
                "at": 1u64,
            })
            .to_string(),
        )
        .unwrap();

        let err = GraphStore::open_read_only(&db_path)
            .map(|_| ())
            .unwrap_err();
        assert!(
            !err.to_string().contains("on purpose"),
            "a marker for another pid must not be used to explain this one: {err}"
        );
    }

    /// The marker is inert once the WAL is gone, so it must not survive to
    /// mislabel a later death.
    #[test]
    fn a_stale_hard_exit_marker_is_cleared_by_an_open_with_no_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        drop(GraphStore::open(&db_path).unwrap());
        let marker = crate::recovery::unclean_exit_marker_path(dir.path());
        std::fs::write(&marker, serde_json::json!({"pid": DEAD_PID}).to_string()).unwrap();

        drop(GraphStore::open(&db_path).unwrap());
        assert!(!marker.exists(), "a WAL-free open must clear it");
    }

    #[test]
    fn open_read_only_on_missing_graph_gives_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");

        let err = match GraphStore::open_read_only(&db_path) {
            Ok(_) => panic!("expected an error opening a nonexistent graph read-only"),
            Err(e) => e,
        };

        assert!(
            err.to_string().contains("run `infigraph index` first"),
            "expected the actionable no-graph-yet message, got: {err}"
        );
    }

    #[test]
    fn open_read_only_succeeds_once_the_graph_has_been_created() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph");
        drop(GraphStore::open(&db_path).unwrap());

        GraphStore::open_read_only(&db_path).unwrap();
    }
}
