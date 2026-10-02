//! Machine-wide caps on work every project decides to do on its own.
//!
//! Every project's daemon, detached `scip-enrich` and foreground `index`
//! decides for itself to run SCIP indexers or to rebuild its graph, and each
//! is heavy: one indexer (scip-typescript, rust-analyzer) takes 1-2 GB and a
//! core or more, and one full reindex of a 1,750-file project holds about
//! 1.9 GB and parses on every core (#150). Rebuilding several projects back
//! to back therefore ran all of it at once: on 2026-10-01 a 14-core, 48 GB
//! machine reached a load of 46 with its swap nearly full.
//!
//! A cap is a [`SlotPool`]: a directory of lock files under `~/.infigraph/`,
//! as many as the cap. Work runs only while it holds one. The lock is an OS
//! file lock, so a slot whose holder dies is free again at once and nothing
//! has to clean up after a crash. There is no queue: waiters poll, and
//! whichever asks first after a slot frees gets it.
//!
//! Two pools exist, one per kind of work, sized from the user layer only:
//! a cap is a property of the machine, not of a project.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::lockfile::{self, LockFile};
use crate::settings_file::ConfigScope;

const SCIP_CATEGORY: &str = "scip";
const MAX_CONCURRENT_INDEXERS: &str = "max_concurrent_indexers";
const INDEX_CATEGORY: &str = "index";
const MAX_CONCURRENT_REINDEXES: &str = "max_concurrent_reindexes";

crate::settings! {
    scip {
        // How many SCIP indexer processes may run at once on this machine,
        // across every project. 0 means no cap.
        max_concurrent_indexers: u64 = 2,
    }
}

crate::settings! {
    index {
        // How many daemons may run a full reindex at once on this machine.
        // 0 means no cap.
        max_concurrent_reindexes: u64 = 2,
    }
}

/// A held slot; dropping it frees the slot.
pub struct Slot {
    _lock: Option<LockFile>,
}

/// Where a cap's slots live, how many there are, and what holds them.
#[derive(Debug, Clone)]
pub struct SlotPool {
    dir: PathBuf,
    max: usize,
    /// Written into a held slot's lock file, so `infigraph ps` and a person
    /// reading the file can tell what holds it.
    role: &'static str,
}

impl SlotPool {
    pub fn new(dir: PathBuf, max: usize) -> Self {
        Self {
            dir,
            max,
            role: "slot",
        }
    }

    /// This machine's SCIP indexer pool: `~/.infigraph/scip-slots/`, sized by
    /// `[scip] max_concurrent_indexers`. `None` when there is no home
    /// directory, which means no cap.
    pub fn scip_indexers() -> Option<Self> {
        let max =
            Scip::resolve_or_default(RawScip::default(), ConfigScope::User).max_concurrent_indexers;
        Self::under_home("scip-slots", max, "scip-indexer")
    }

    /// This machine's full-reindex pool: `~/.infigraph/reindex-slots/`, sized
    /// by `[index] max_concurrent_reindexes`. `None` when there is no home
    /// directory, which means no cap.
    pub fn full_reindexes() -> Option<Self> {
        let max = Index::resolve_or_default(RawIndex::default(), ConfigScope::User)
            .max_concurrent_reindexes;
        Self::under_home("reindex-slots", max, "full-reindex")
    }

    fn under_home(dir_name: &str, max: u64, role: &'static str) -> Option<Self> {
        Some(Self {
            dir: crate::settings_file::user_infigraph_dir()?.join(dir_name),
            max: max as usize,
            role,
        })
    }

    pub fn max(&self) -> usize {
        self.max
    }

    /// Claims a free slot without waiting. `Ok(None)` when every slot is
    /// held. A pool of size 0 is uncapped and always grants one.
    pub fn try_claim(&self) -> Result<Option<Slot>> {
        if self.max == 0 {
            return Ok(Some(Slot { _lock: None }));
        }
        std::fs::create_dir_all(&self.dir)?;
        for i in 0..self.max {
            if let Some(lock) = lockfile::try_acquire(&slot_path(&self.dir, i), self.role)? {
                return Ok(Some(Slot { _lock: Some(lock) }));
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
    crate::settings::env_name(SCIP_CATEGORY, MAX_CONCURRENT_INDEXERS)
}

/// The env var that overrides `[index] max_concurrent_reindexes`.
pub fn max_concurrent_reindexes_env_name() -> String {
    crate::settings::env_name(INDEX_CATEGORY, MAX_CONCURRENT_REINDEXES)
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
        let pool = SlotPool::scip_indexers().expect("a home is pinned");
        assert_eq!(pool.max(), 2, "default");
        assert!(pool.dir.starts_with(std::env::var("HOME").unwrap()));
        std::env::set_var(max_concurrent_indexers_env_name(), "5");
        let overridden = SlotPool::scip_indexers().unwrap().max();
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

    #[test]
    fn the_reindex_pool_has_its_own_slots_and_setting() {
        use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        std::env::remove_var(max_concurrent_reindexes_env_name());
        std::env::remove_var(max_concurrent_indexers_env_name());
        assert_eq!(
            max_concurrent_reindexes_env_name(),
            "INFIGRAPH_INDEX_MAX_CONCURRENT_REINDEXES"
        );
        let reindexes = SlotPool::full_reindexes().expect("a home is pinned");
        assert_eq!(reindexes.max(), 2, "default");

        // Filling one pool leaves the other untouched.
        let indexers = SlotPool::scip_indexers().unwrap();
        let _held: Vec<_> = (0..2)
            .map(|_| indexers.try_claim().unwrap().unwrap())
            .collect();
        assert!(indexers.try_claim().unwrap().is_none());
        assert!(reindexes.try_claim().unwrap().is_some());
    }
}
