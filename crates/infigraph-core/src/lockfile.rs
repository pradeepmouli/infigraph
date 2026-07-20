//! Lock-file mechanics shared by all infigraph locks (graph write lock,
//! and future operation/session/registry locks).
//!
//! Model: a kernel advisory flock (via `fs2`) is the source of truth for
//! "held" — flocks release automatically when the holder dies, so no
//! liveness polling is needed. The JSON identity payload written into the
//! lock file exists for diagnostics (who holds it, since when, built from
//! what) and is never trusted for liveness decisions. Conservative rule:
//! a held flock with an unreadable payload is an *unknown holder* —
//! bounded-wait then `Busy`, never broken.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Identity payload stamped into a held lock file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub role: String,
    pub build_hash: String,
    /// Unix epoch seconds at acquisition.
    pub acquired_at: u64,
}

impl LockInfo {
    pub fn current(role: &str) -> Self {
        Self {
            pid: std::process::id(),
            role: role.to_string(),
            build_hash: crate::build_hash().to_string(),
            acquired_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

/// Returned when a lock could not be acquired within the wait budget.
#[derive(Debug)]
pub struct Busy {
    pub lock_path: PathBuf,
    pub holder: Option<LockInfo>,
    pub waited: Duration,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.holder {
            Some(h) => {
                let held_for = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs().saturating_sub(h.acquired_at))
                    .unwrap_or(0);
                write!(
                    f,
                    "{} is locked by {} (PID {}), held {}s — waited {}s, giving up",
                    self.lock_path.display(),
                    h.role,
                    h.pid,
                    held_for,
                    self.waited.as_secs()
                )
            }
            None => write!(
                f,
                "{} is locked by an unknown holder — waited {}s, giving up",
                self.lock_path.display(),
                self.waited.as_secs()
            ),
        }
    }
}

impl std::error::Error for Busy {}
