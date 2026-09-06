use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
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

    pub fn save(&self, session: &SessionData) -> Result<()> {
        let path = self.session_path(&session.id);
        let json = serde_json::to_string_pretty(session)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    pub fn load(&self, session_id: &str) -> Result<Option<SessionData>> {
        let path = self.session_path(session_id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let session: SessionData = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse session: {}", path.display()))?;
        Ok(Some(session))
    }

    pub fn list_all(&self) -> Result<Vec<SessionData>> {
        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(&self.sessions_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if (name_str.starts_with("session_") || name_str.starts_with("named_"))
                && name_str.ends_with(".json")
            {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(session) = serde_json::from_str::<SessionData>(&content) {
                        sessions.push(session);
                    }
                }
            }
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(sessions)
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
            let get = |i: usize| {
                row.get(i)
                    .map(|v| v.to_string())
                    .unwrap_or_default()
                    .trim_matches('"')
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
            let json = serde_json::to_string_pretty(session)?;
            std::fs::write(&json_path, json)?;
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
