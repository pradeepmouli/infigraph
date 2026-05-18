use anyhow::Result;
use kuzu::{Connection, Database, SystemConfig};
use std::path::Path;

const SESSION_SCHEMA: &[&str] = &["CREATE NODE TABLE IF NOT EXISTS Session(
        id STRING,
        summary STRING,
        pending_tasks STRING,
        decisions STRING,
        files_touched STRING,
        constraints STRING,
        assumptions STRING,
        blockers STRING,
        created_at INT64,
        updated_at INT64,
        PRIMARY KEY(id)
    )"];

const SESSION_MIGRATIONS: &[&str] = &[
    "ALTER TABLE Session ADD constraints STRING DEFAULT ''",
    "ALTER TABLE Session ADD assumptions STRING DEFAULT ''",
    "ALTER TABLE Session ADD blockers STRING DEFAULT ''",
];

pub struct SessionStore {
    db: Database,
}

impl SessionStore {
    pub fn open(project_root: &Path) -> Result<Self> {
        let db_path = project_root.join(".infigraph").join("sessions").join("db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::new(&db_path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open session db: {e}"))?;
        let store = Self { db };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.connection()?;
        for ddl in SESSION_SCHEMA {
            conn.query(ddl)
                .map_err(|e| anyhow::anyhow!("session schema error: {e}\n  DDL: {ddl}"))?;
        }
        for migration in SESSION_MIGRATIONS {
            let _ = conn.query(migration);
        }
        Ok(())
    }

    pub fn connection(&self) -> Result<Connection<'_>> {
        Connection::new(&self.db)
            .map_err(|e| anyhow::anyhow!("failed to create session connection: {e}"))
    }

    pub fn raw_query(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let conn = self.connection()?;
        let result = conn
            .query(cypher)
            .map_err(|e| anyhow::anyhow!("session query failed: {e}"))?;
        let mut rows = Vec::new();
        for row in result {
            rows.push(row.iter().map(|v| v.to_string()).collect());
        }
        Ok(rows)
    }
}
