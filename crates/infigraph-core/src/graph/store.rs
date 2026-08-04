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
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open kuzu db: {e}"))?;
        let store = Self { db, lock_path };
        let lock = WriteLock::acquire_with_timeout(&store.lock_path, timeout)?;
        store.init_schema(&lock)?;
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
        let lock_path = db_lock_path(path);
        // `throw_on_wal_replay_failure` defaults to true (unset here): a WAL
        // replay failure now surfaces as an error instead of being silently
        // tolerated and served as a torn base image.
        let config = SystemConfig::default().read_only(true);
        let db = Database::new(path, config).map_err(|e| {
            let msg = format!("failed to open kuzu db (read-only): {e}");
            if msg.to_lowercase().contains("wal") {
                anyhow::Error::new(GraphCorruption { detail: msg })
            } else {
                anyhow::anyhow!(msg)
            }
        })?;
        Ok(Self { db, lock_path })
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
        Ok(())
    }

    pub fn connection(&self) -> Result<Connection<'_>> {
        Connection::new(&self.db).map_err(|e| anyhow::anyhow!("failed to create connection: {e}"))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
