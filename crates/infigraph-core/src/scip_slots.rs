//! A machine-wide cap on concurrent SCIP indexer processes.
//!
//! Every project's daemon, detached `scip-enrich` and foreground `index`
//! decides on its own to run SCIP indexers, and one indexer (scip-typescript,
//! rust-analyzer) takes 1-2 GB and a core or more. Rebuilding several projects
//! back to back therefore ran all their enrichments at once: on 2026-10-01 a
//! 14-core, 48 GB machine reached a load of 46 with its swap nearly full.
//!
//! The cap is a pool of `[scip] max_concurrent_indexers` lock files under
//! `~/.infigraph/scip-slots/`. An indexer runs only while it holds one. The
//! lock is an OS file lock, so a slot whose holder dies is free again at
//! once; nothing has to clean up after a crash.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::lockfile::{self, LockFile};
use crate::settings_file::ConfigScope;

const CATEGORY: &str = "scip";
const MAX_CONCURRENT_INDEXERS: &str = "max_concurrent_indexers";

crate::settings! {
    scip {
        // How many SCIP indexer processes may run at once on this machine,
        // across every project. 0 means no cap. Read from the user layer
        // only: it is a property of the machine, not of a project.
        max_concurrent_indexers: u64 = 2,
    }
}

/// A held indexer slot; dropping it frees the slot.
pub struct IndexerSlot {
    _lock: Option<LockFile>,
}

/// Where the slots live and how many there are.
#[derive(Debug, Clone)]
pub struct SlotPool {
    dir: PathBuf,
    max: usize,
}

impl SlotPool {
    pub fn new(dir: PathBuf, max: usize) -> Self {
        Self { dir, max }
    }

    /// This machine's pool: `~/.infigraph/scip-slots/`, sized by
    /// `[scip] max_concurrent_indexers`. `None` when there is no home
    /// directory, which means no cap.
    pub fn machine() -> Option<Self> {
        let dir = crate::settings_file::user_infigraph_dir()?.join("scip-slots");
        let max =
            Scip::resolve_or_default(RawScip::default(), ConfigScope::User).max_concurrent_indexers;
        Some(Self::new(dir, max as usize))
    }

    pub fn max(&self) -> usize {
        self.max
    }

    /// Claims a free slot without waiting. `Ok(None)` when every slot is
    /// held. A pool of size 0 is uncapped and always grants one.
    pub fn try_claim(&self) -> Result<Option<IndexerSlot>> {
        if self.max == 0 {
            return Ok(Some(IndexerSlot { _lock: None }));
        }
        std::fs::create_dir_all(&self.dir)?;
        for i in 0..self.max {
            if let Some(lock) = lockfile::try_acquire(&slot_path(&self.dir, i), "scip-indexer")? {
                return Ok(Some(IndexerSlot { _lock: Some(lock) }));
            }
        }
        Ok(None)
    }
}

fn slot_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("slot-{i}.lock"))
}

/// The env var that overrides `[scip] max_concurrent_indexers`, derived from
/// the settings definition.
pub fn max_concurrent_indexers_env_name() -> String {
    crate::settings::env_name(CATEGORY, MAX_CONCURRENT_INDEXERS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_pool_grants_nothing_until_a_slot_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = SlotPool::new(tmp.path().join("slots"), 2);
        let a = pool.try_claim().unwrap().expect("first slot");
        let _b = pool.try_claim().unwrap().expect("second slot");
        assert!(
            pool.try_claim().unwrap().is_none(),
            "a third indexer got a slot from a pool of two"
        );
        drop(a);
        assert!(
            pool.try_claim().unwrap().is_some(),
            "a dropped slot was not freed"
        );
    }

    #[test]
    fn a_pool_of_zero_is_uncapped() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = SlotPool::new(tmp.path().join("slots"), 0);
        let held: Vec<_> = (0..5).map(|_| pool.try_claim().unwrap()).collect();
        assert!(held.iter().all(Option::is_some));
        assert!(
            !tmp.path().join("slots").exists(),
            "an uncapped pool made a dir"
        );
    }

    #[test]
    fn the_machine_pool_reads_the_user_layer_and_env() {
        use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        std::env::remove_var(max_concurrent_indexers_env_name());
        let pool = SlotPool::machine().expect("a home is pinned");
        assert_eq!(pool.max(), 2, "default");
        assert!(pool.dir.starts_with(std::env::var("HOME").unwrap()));
        std::env::set_var(max_concurrent_indexers_env_name(), "5");
        let overridden = SlotPool::machine().unwrap().max();
        std::env::remove_var(max_concurrent_indexers_env_name());
        assert_eq!(overridden, 5);
    }

    #[test]
    fn the_env_name_comes_from_the_definition() {
        assert_eq!(
            max_concurrent_indexers_env_name(),
            "INFIGRAPH_SCIP_MAX_CONCURRENT_INDEXERS"
        );
    }
}
