use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::daemon_protocol::write_atomic;
use crate::degraded::{live, DegradedMode, SESSIONS_UNREADABLE};
use crate::lockfile::{self, LockFile};

/// How long a session write waits for `sessions.lock` before giving up.
pub const SESSION_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Reads an epoch field as whole seconds. Another writer (the hook's file
/// fallback, in Python) may put `time.time()` there, `1792333398.318963`:
/// that is read, truncated, and written back as an integer by the next save.
/// Anything that is not a non-negative number is an error naming `field`.
struct EpochSeconds(&'static str);

impl<'de> serde::de::Visitor<'de> for EpochSeconds {
    type Value = i64;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "a non-negative number of epoch seconds for `{}`", self.0)
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<i64, E> {
        if v < 0 {
            return Err(E::custom(format!("`{}` is negative ({v})", self.0)));
        }
        Ok(v)
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<i64, E> {
        i64::try_from(v).map_err(|_| E::custom(format!("`{}` is out of range ({v})", self.0)))
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<i64, E> {
        if !v.is_finite() || !(0.0..9.0e18).contains(&v) {
            return Err(E::custom(format!(
                "`{}` is not a usable number of seconds ({v})",
                self.0
            )));
        }
        Ok(v as i64)
    }
}

macro_rules! epoch_field {
    ($name:ident, $field:literal) => {
        fn $name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
            d.deserialize_any(EpochSeconds($field))
        }
    };
}
epoch_field!(created_at_seconds, "created_at");
epoch_field!(updated_at_seconds, "updated_at");
epoch_field!(last_accessed_seconds, "last_accessed");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub id: String,
    pub summary: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub pending_tasks: String,
    #[serde(default)]
    pub decisions: String,
    #[serde(default)]
    pub files_touched: String,
    #[serde(default)]
    pub constraints: String,
    #[serde(default)]
    pub assumptions: String,
    #[serde(default)]
    pub blockers: String,
    #[serde(default, deserialize_with = "created_at_seconds")]
    pub created_at: i64,
    #[serde(default, deserialize_with = "updated_at_seconds")]
    pub updated_at: i64,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default, deserialize_with = "last_accessed_seconds")]
    pub last_accessed: i64,
}

const DECAY_PER_WEEK: f32 = 0.05;
const ARCHIVE_THRESHOLD: f32 = 0.3;
const INITIAL_CONFIDENCE: f32 = 0.7;

impl SessionData {
    pub fn compute_confidence(&self, now_epoch: i64) -> f32 {
        let base = if self.confidence > 0.0 {
            self.confidence
        } else {
            INITIAL_CONFIDENCE
        };
        let last = if self.last_accessed > 0 {
            self.last_accessed
        } else {
            self.updated_at.max(self.created_at)
        };
        let weeks_elapsed = ((now_epoch - last) as f32 / 604800.0).max(0.0);
        (base - DECAY_PER_WEEK * weeks_elapsed).clamp(0.0, 1.0)
    }

    pub fn is_archived(&self, now_epoch: i64) -> bool {
        self.compute_confidence(now_epoch) < ARCHIVE_THRESHOLD
    }

    pub fn touch(&mut self, now_epoch: i64) {
        self.last_accessed = now_epoch;
    }
}

/// A session file that did not read, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub file: PathBuf,
    pub reason: String,
}

/// What a read of the sessions directory found.
#[derive(Debug, Clone)]
pub struct Scan {
    pub sessions: Vec<SessionData>,
    pub skipped: Vec<Skipped>,
}

pub struct SessionStore {
    sessions_dir: PathBuf,
}

impl SessionStore {
    pub fn open(project_root: &Path) -> Result<Self> {
        let sessions_dir = project_root.join(".infigraph").join("sessions");
        std::fs::create_dir_all(&sessions_dir)?;
        let store = Self { sessions_dir };
        store.migrate_from_kuzu()?;
        Ok(store)
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// The lock a read-modify-write of a session holds: `save` replaces the
    /// file atomically, so a reader never needs it, but two writers that each
    /// load, change and save would lose one update without it. Not reentrant:
    /// a caller already holding it must not call [`touch_session`].
    ///
    /// [`touch_session`]: Self::touch_session
    pub fn lock(&self) -> Result<LockFile> {
        lockfile::acquire(
            &self.sessions_dir.join("sessions.lock"),
            "session-write",
            SESSION_LOCK_TIMEOUT,
        )
    }

    /// Replaces the session's file in one rename, so a concurrent `load` sees
    /// the old session or the new one, never an empty or half-written file
    /// (`save_session` once failed with "failed to parse session" that way).
    pub fn save(&self, session: &SessionData) -> Result<()> {
        let path = self.session_path(&session.id);
        let json = serde_json::to_string_pretty(session)?;
        write_atomic(&path, &json)
    }

    pub fn load(&self, session_id: &str) -> Result<Option<SessionData>> {
        let path = self.session_path(session_id);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // The parser's own words go in the message: `with_context` would hide
        // them from every caller that prints the error with `{}`.
        let session: SessionData = serde_json::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse session {}: {e}", path.display()))?;
        Ok(Some(session))
    }

    /// Every session that reads, newest first, and every session file that
    /// does not, with why. The skipped files are also reported as the degraded
    /// mode `sessions-unreadable` (and cleared once all read again), so a
    /// session that vanishes from a list says so where a person looks.
    pub fn scan(&self) -> Result<Scan> {
        let mut sessions = Vec::new();
        let mut skipped = Vec::new();
        for entry in std::fs::read_dir(&self.sessions_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if (name_str.starts_with("session_") || name_str.starts_with("named_"))
                && name_str.ends_with(".json")
            {
                let read = std::fs::read_to_string(entry.path())
                    .map_err(|e| e.to_string())
                    .and_then(|content| {
                        serde_json::from_str::<SessionData>(&content).map_err(|e| e.to_string())
                    });
                match read {
                    Ok(session) => sessions.push(session),
                    Err(reason) => skipped.push(Skipped {
                        file: entry.path(),
                        reason,
                    }),
                }
            }
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        skipped.sort_by(|a, b| a.file.cmp(&b.file));
        self.report(&skipped);
        Ok(Scan { sessions, skipped })
    }

    pub fn list_all(&self) -> Result<Vec<SessionData>> {
        Ok(self.scan()?.sessions)
    }

    /// The project this store belongs to, when its directory is
    /// `<project>/.infigraph/sessions`.
    fn project_root(&self) -> Option<&Path> {
        let infigraph = self.sessions_dir.parent()?;
        (infigraph.file_name()? == ".infigraph")
            .then(|| infigraph.parent())
            .flatten()
    }

    fn report(&self, skipped: &[Skipped]) {
        let Some(root) = self.project_root() else {
            return;
        };
        match skipped.first() {
            Some(first) => live::set(
                root,
                DegradedMode::SessionsUnreadable {
                    count: skipped.len(),
                    first: format!(
                        "{}: {}",
                        first
                            .file
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        first.reason
                    ),
                },
            ),
            None => live::clear(root, SESSIONS_UNREADABLE),
        }
    }

    pub fn list_active(&self, now_epoch: i64) -> Result<Vec<SessionData>> {
        let all = self.list_all()?;
        Ok(all
            .into_iter()
            .filter(|s| !s.is_archived(now_epoch))
            .collect())
    }

    pub fn purge_expired(&self, now_epoch: i64) -> Result<Vec<String>> {
        let all = self.list_all()?;
        let mut deleted = Vec::new();
        for session in &all {
            if session.compute_confidence(now_epoch) < 0.1 {
                self.delete(&session.id)?;
                deleted.push(session.id.clone());
            }
        }
        Ok(deleted)
    }

    pub fn touch_session(&self, session_id: &str) -> Result<()> {
        let _lock = self.lock()?;
        if let Some(mut session) = self.load(session_id)? {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            session.touch(now);
            self.save(&session)?;
        }
        Ok(())
    }

    pub fn list_recent(&self, limit: usize) -> Result<Vec<SessionData>> {
        let mut all = self.list_all()?;
        all.truncate(limit);
        Ok(all)
    }

    pub fn list_by_updated(&self) -> Result<Vec<SessionData>> {
        let mut sessions = self.list_all()?;
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(sessions)
    }

    pub fn load_by_name(&self, name: &str) -> Result<Option<SessionData>> {
        let id = format!("named_{}", name.to_lowercase().replace(' ', "_"));
        self.load(&id)
    }

    pub fn delete(&self, session_id: &str) -> Result<()> {
        let path = self.session_path(session_id);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Buffer-pool budget for the legacy sessions read. Small on purpose: this
    /// runs one query against a small file, once, during migration.
    const LEGACY_SESSION_READ_POOL_BYTES: u64 = 64 * 1024 * 1024;

    fn read_kuzu_sessions(db_path: &Path) -> Vec<SessionData> {
        // Read-only, and with a bounded pool. This is a one-off migration
        // READ of a legacy store: opening it writable takes Kuzu's
        // cross-process writer-exclusivity lock for no reason, and would
        // create the database as a side effect if it were missing -- a read
        // that creates a database is a bug, not a feature. `migrate_from_kuzu`
        // already returns early unless the path exists, so read-only cannot
        // fail here for want of creation. The default pool is auto-detected
        // from system RAM, which is wildly out of proportion for one query
        // against a legacy sessions file.
        let config = kuzu::SystemConfig::default()
            .read_only(true)
            .buffer_pool_size(Self::LEGACY_SESSION_READ_POOL_BYTES);
        let db = match kuzu::Database::new(db_path, config) {
            Ok(db) => db,
            Err(_) => return Vec::new(),
        };
        let conn = match kuzu::Connection::new(&db) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let query = "MATCH (s:Session) RETURN s.id, s.summary, s.pending_tasks, s.decisions, \
                     s.files_touched, s.created_at, s.updated_at, s.constraints, s.assumptions, s.blockers";
        let result = match conn.query(query) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut collected = Vec::new();
        for row in result {
            // #181: unlike the `QueryExec`-backed reads, this queries the
            // connection directly, so `row.get(i)` is a `kuzu::Value` rather
            // than a `String`. `raw_string` takes the stored string as-is;
            // the previous `trim_matches('"')` stripped any quote a summary
            // or field genuinely began or ended with.
            let get = |i: usize| {
                row.get(i)
                    .map(crate::graph::store_util::raw_string)
                    .unwrap_or_default()
                    .to_string()
            };
            let id = get(0);
            if id.is_empty() {
                continue;
            }
            let created: i64 = get(5).parse().unwrap_or(0);
            let updated: i64 = get(6).parse().unwrap_or(created);
            collected.push(SessionData {
                id,
                name: String::new(),
                summary: get(1),
                pending_tasks: get(2),
                decisions: get(3),
                files_touched: get(4),
                constraints: get(7),
                assumptions: get(8),
                blockers: get(9),
                created_at: created,
                updated_at: updated,
                confidence: 0.0,
                last_accessed: 0,
            });
        }
        collected
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.json"))
    }

    pub fn open_dir(sessions_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(sessions_dir)?;
        Ok(Self {
            sessions_dir: sessions_dir.to_path_buf(),
        })
    }

    fn migrate_from_kuzu(&self) -> Result<()> {
        let db_path = self.sessions_dir.join("db");
        if !db_path.exists() {
            return Ok(());
        }

        let sessions = Self::read_kuzu_sessions(&db_path);

        let mut count = 0u32;
        for session in &sessions {
            let json_path = self.session_path(&session.id);
            if json_path.exists() {
                continue;
            }
            self.save(session)?;
            count += 1;
        }

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(self.sessions_dir.join(".migrated_to_json"));
        let _ = std::fs::remove_file(self.sessions_dir.join("latest_session.json"));
        eprintln!("Migrated {count} session(s) from KuzuDB to JSON files, removed old session DB");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session(id: &str, created_at: i64, updated_at: i64) -> SessionData {
        SessionData {
            id: id.to_string(),
            name: String::new(),
            summary: format!("work on {id}"),
            pending_tasks: String::new(),
            decisions: String::new(),
            files_touched: String::new(),
            constraints: String::new(),
            assumptions: String::new(),
            blockers: String::new(),
            created_at,
            updated_at,
            confidence: 0.0,
            last_accessed: 0,
        }
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_dir(dir.path()).unwrap();
        let s = make_session("session_2026-06-08", 1000, 2000);
        store.save(&s).unwrap();
        let loaded = store.load("session_2026-06-08").unwrap().unwrap();
        assert_eq!(loaded.id, "session_2026-06-08");
        assert_eq!(loaded.updated_at, 2000);
    }

    #[test]
    fn test_list_all_sorted_by_created() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_dir(dir.path()).unwrap();
        store
            .save(&make_session("session_2026-06-05", 100, 200))
            .unwrap();
        store
            .save(&make_session("session_2026-06-07", 300, 400))
            .unwrap();
        store
            .save(&make_session("session_2026-06-06", 200, 500))
            .unwrap();

        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, "session_2026-06-07");
        assert_eq!(all[1].id, "session_2026-06-06");
        assert_eq!(all[2].id, "session_2026-06-05");
    }

    #[test]
    fn test_list_by_updated_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_dir(dir.path()).unwrap();
        store
            .save(&make_session("session_2026-06-05", 100, 500))
            .unwrap();
        store
            .save(&make_session("session_2026-06-07", 300, 300))
            .unwrap();
        store
            .save(&make_session("session_2026-06-06", 200, 400))
            .unwrap();

        let sorted = store.list_by_updated().unwrap();
        assert_eq!(sorted[0].id, "session_2026-06-05");
        assert_eq!(sorted[1].id, "session_2026-06-06");
        assert_eq!(sorted[2].id, "session_2026-06-07");
    }

    #[test]
    fn test_list_recent_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_dir(dir.path()).unwrap();
        store
            .save(&make_session("session_2026-06-05", 100, 100))
            .unwrap();
        store
            .save(&make_session("session_2026-06-06", 200, 200))
            .unwrap();
        store
            .save(&make_session("session_2026-06-07", 300, 300))
            .unwrap();

        let recent = store.list_recent(2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, "session_2026-06-07");
        assert_eq!(recent[1].id, "session_2026-06-06");
    }

    #[test]
    fn test_delete_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_dir(dir.path()).unwrap();
        store
            .save(&make_session("session_2026-06-08", 100, 100))
            .unwrap();
        assert!(store.load("session_2026-06-08").unwrap().is_some());
        store.delete("session_2026-06-08").unwrap();
        assert!(store.load("session_2026-06-08").unwrap().is_none());
    }

    #[test]
    fn test_load_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_dir(dir.path()).unwrap();
        assert!(store.load("session_nope").unwrap().is_none());
    }

    #[test]
    fn test_confidence_initial_default() {
        let s = make_session("s1", 1000, 1000);
        // confidence=0.0 → uses INITIAL_CONFIDENCE (0.7)
        let conf = s.compute_confidence(1000);
        assert!(
            (conf - 0.7).abs() < 0.01,
            "initial confidence should be 0.7, got {conf}"
        );
    }

    #[test]
    fn test_confidence_decays_over_weeks() {
        let s = make_session("s1", 1000, 1000);
        let one_week = 604800;
        // After 1 week: 0.7 - 0.05 = 0.65
        let conf = s.compute_confidence(1000 + one_week);
        assert!((conf - 0.65).abs() < 0.01, "after 1 week: {conf}");
        // After 4 weeks: 0.7 - 0.20 = 0.50
        let conf = s.compute_confidence(1000 + 4 * one_week);
        assert!((conf - 0.50).abs() < 0.01, "after 4 weeks: {conf}");
        // After 8 weeks: 0.7 - 0.40 = 0.30
        let conf = s.compute_confidence(1000 + 8 * one_week);
        assert!((conf - 0.30).abs() < 0.01, "after 8 weeks: {conf}");
    }

    #[test]
    fn test_confidence_archived_threshold() {
        let s = make_session("s1", 1000, 1000);
        let one_week = 604800;
        // After 7 weeks: 0.35 → not archived
        assert!(!s.is_archived(1000 + 7 * one_week));
        // After 9 weeks: 0.25 → archived
        assert!(s.is_archived(1000 + 9 * one_week));
    }

    #[test]
    fn test_confidence_clamps_to_zero() {
        let s = make_session("s1", 1000, 1000);
        let one_week = 604800;
        // After 100 weeks: should clamp to 0.0, not go negative
        let conf = s.compute_confidence(1000 + 100 * one_week);
        assert!(conf >= 0.0, "confidence should not go negative: {conf}");
        assert!(conf == 0.0, "confidence should be 0.0: {conf}");
    }

    #[test]
    fn test_confidence_explicit_value_used() {
        let mut s = make_session("s1", 1000, 1000);
        s.confidence = 1.0; // user-confirmed
        s.last_accessed = 1000;
        let one_week = 604800;
        // After 2 weeks: 1.0 - 0.10 = 0.90
        let conf = s.compute_confidence(1000 + 2 * one_week);
        assert!((conf - 0.90).abs() < 0.01, "explicit confidence: {conf}");
    }

    #[test]
    fn test_touch_updates_last_accessed() {
        let mut s = make_session("s1", 1000, 1000);
        assert_eq!(s.last_accessed, 0);
        s.touch(5000);
        assert_eq!(s.last_accessed, 5000);
        // Confidence now computed from last_accessed
        let conf = s.compute_confidence(5000);
        assert!((conf - 0.7).abs() < 0.01, "after touch: {conf}");
    }

    #[test]
    fn test_list_active_filters_archived() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open_dir(dir.path()).unwrap();
        let one_week = 604800i64;
        let now = 100 * one_week;

        // Recent session — should be active
        let mut recent = make_session("session_recent", now - one_week, now - one_week);
        recent.last_accessed = now - one_week;
        store.save(&recent).unwrap();

        // Old session — should be archived (9+ weeks old, conf < 0.3)
        let mut old = make_session("session_old", now - 10 * one_week, now - 10 * one_week);
        old.last_accessed = now - 10 * one_week;
        store.save(&old).unwrap();

        let active = store.list_active(now).unwrap();
        assert_eq!(active.len(), 1, "should filter out archived session");
        assert_eq!(active[0].id, "session_recent");
    }
}
