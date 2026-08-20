//! Registry garbage collection (R7.1, docs/DESIGN-hardening.md §7.1,
//! github.com/pradeepmouli/infigraph#12).
//!
//! `~/.infigraph/registry.json` accumulates entries forever: a project
//! that gets deleted, moved, or abandoned keeps its row (and its implied
//! "this path is indexed" claim) until something evicts it -- the I-7
//! incident grew 69 entries / 8.3 GB of orphaned state this way. This
//! module computes and applies the eviction plan; the CLI (`infigraph gc`)
//! owns persistence and the R6.3 audit lines, so planning stays a pure,
//! testable function of (registry, clock).
//!
//! Two eviction classes, deliberately asymmetric:
//! - **Missing path** (default): the registered directory no longer exists.
//!   Nothing to lose -- the entry describes a project that is gone.
//! - **Stale age** (opt-in via `stale_days`): the project exists but its
//!   graph hasn't been rebuilt in N days (judged by the graph file's
//!   mtime -- `RepoEntry` carries no timestamp, and the mtime is ground
//!   truth the entry can't get wrong). Evicting a *live* project's
//!   registry entry is recoverable (`infigraph index` re-registers) but
//!   still user-visible, so it never happens unless explicitly requested.

use crate::multi::Registry;
use std::path::PathBuf;
use std::time::SystemTime;

/// Why an entry is being evicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcReason {
    /// The registered path no longer exists on disk.
    PathMissing,
    /// The project's graph was last rebuilt this many days ago (>= the
    /// caller's `stale_days` threshold).
    Stale { days_since_index: u64 },
}

impl std::fmt::Display for GcReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GcReason::PathMissing => write!(f, "path no longer exists"),
            GcReason::Stale { days_since_index } => {
                write!(f, "last indexed {days_since_index} days ago")
            }
        }
    }
}

/// One planned eviction.
#[derive(Debug, Clone)]
pub struct GcCandidate {
    pub name: String,
    pub path: PathBuf,
    pub reason: GcReason,
}

/// The full computed plan. Pure data -- nothing has been mutated yet.
#[derive(Debug, Default)]
pub struct GcPlan {
    pub evictions: Vec<GcCandidate>,
    /// `(group_name, repo_name)` pairs whose repo will no longer exist in
    /// the registry after the evictions apply -- includes members that
    /// were ALREADY dangling before this run (a prior manual edit or
    /// partial failure), so executing the plan always leaves groups
    /// internally consistent.
    pub dangling_group_members: Vec<(String, String)>,
}

impl GcPlan {
    pub fn is_empty(&self) -> bool {
        self.evictions.is_empty() && self.dangling_group_members.is_empty()
    }
}

/// Seconds per day, factored for the tests' fabricated clocks.
const DAY_SECS: u64 = 86_400;

/// Compute the eviction plan. `now` is injected (not read from the clock)
/// so staleness is a pure function testable with fabricated times.
pub fn plan_registry_gc(registry: &Registry, stale_days: Option<u64>, now: SystemTime) -> GcPlan {
    let mut evictions = Vec::new();

    for entry in registry.repos.values() {
        if !entry.path.exists() {
            evictions.push(GcCandidate {
                name: entry.name.clone(),
                path: entry.path.clone(),
                reason: GcReason::PathMissing,
            });
            continue;
        }
        if let Some(threshold_days) = stale_days {
            // The graph file's mtime is when the project was last (re)built.
            // A registered path with no graph file can't be dated -- skip it
            // rather than guess (conservative: a false eviction here is the
            // failure mode, not a false keep).
            let graph_path = entry.path.join(".infigraph").join("graph");
            let Ok(meta) = std::fs::metadata(&graph_path) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            let Ok(age) = now.duration_since(mtime) else {
                continue; // mtime in the future -- clock skew, don't evict
            };
            let days = age.as_secs() / DAY_SECS;
            if days >= threshold_days {
                evictions.push(GcCandidate {
                    name: entry.name.clone(),
                    path: entry.path.clone(),
                    reason: GcReason::Stale {
                        days_since_index: days,
                    },
                });
            }
        }
    }

    // Group members dangling after (or already before) the evictions.
    let evicted_names: std::collections::HashSet<&str> =
        evictions.iter().map(|c| c.name.as_str()).collect();
    let mut dangling = Vec::new();
    for (group_name, group) in &registry.groups {
        for repo_name in &group.repos {
            let gone_already = !registry.repos.contains_key(repo_name);
            let being_evicted = evicted_names.contains(repo_name.as_str());
            if gone_already || being_evicted {
                dangling.push((group_name.clone(), repo_name.clone()));
            }
        }
    }
    dangling.sort();

    GcPlan {
        evictions,
        dangling_group_members: dangling,
    }
}

/// Apply `plan` to the in-memory registry: remove evicted repos and prune
/// their names from every group's member list. Persistence (and the R6.3
/// audit lines recording the destructive act) belong to the caller --
/// nothing here touches disk, so a failed save can't leave the audit and
/// the registry disagreeing about what happened.
pub fn execute_registry_gc(registry: &mut Registry, plan: &GcPlan) {
    for candidate in &plan.evictions {
        registry.repos.remove(&candidate.name);
    }
    for (group_name, repo_name) in &plan.dangling_group_members {
        if let Some(group) = registry.groups.get_mut(group_name) {
            group.repos.retain(|r| r != repo_name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi::{Group, RepoEntry};
    use std::time::Duration;

    fn entry(name: &str, path: PathBuf) -> RepoEntry {
        RepoEntry {
            name: name.to_string(),
            path,
            languages: vec![],
            symbol_count: 0,
            module_count: 0,
            last_indexed_commit: None,
        }
    }

    fn registry_with(entries: Vec<RepoEntry>) -> Registry {
        let mut r = Registry::default();
        for e in entries {
            r.repos.insert(e.name.clone(), e);
        }
        r
    }

    /// Creates a real project dir with a graph file and returns its path
    /// plus the graph's mtime (the "last indexed" instant staleness is
    /// judged against).
    fn live_project(tmp: &tempfile::TempDir, name: &str) -> (PathBuf, SystemTime) {
        let path = tmp.path().join(name);
        std::fs::create_dir_all(path.join(".infigraph")).unwrap();
        let graph = path.join(".infigraph").join("graph");
        std::fs::write(&graph, b"graph").unwrap();
        let mtime = std::fs::metadata(&graph).unwrap().modified().unwrap();
        (path, mtime)
    }

    #[test]
    fn missing_path_is_evicted_and_live_path_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let (live, _) = live_project(&tmp, "live");
        let registry = registry_with(vec![
            entry("live", live),
            entry("gone", tmp.path().join("does-not-exist")),
        ]);

        let plan = plan_registry_gc(&registry, None, SystemTime::now());

        assert_eq!(plan.evictions.len(), 1);
        assert_eq!(plan.evictions[0].name, "gone");
        assert_eq!(plan.evictions[0].reason, GcReason::PathMissing);
    }

    #[test]
    fn staleness_is_opt_in_and_judged_by_graph_mtime_against_injected_now() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, mtime) = live_project(&tmp, "old");
        let registry = registry_with(vec![entry("old", path)]);

        // Without stale_days, age never evicts.
        let now = mtime + Duration::from_secs(365 * DAY_SECS);
        assert!(plan_registry_gc(&registry, None, now).is_empty());

        // 91 days old vs a 90-day threshold: evicted, with the age reported.
        let now = mtime + Duration::from_secs(91 * DAY_SECS);
        let plan = plan_registry_gc(&registry, Some(90), now);
        assert_eq!(plan.evictions.len(), 1);
        assert_eq!(
            plan.evictions[0].reason,
            GcReason::Stale {
                days_since_index: 91
            }
        );

        // 89 days old vs the same threshold: kept.
        let now = mtime + Duration::from_secs(89 * DAY_SECS);
        assert!(plan_registry_gc(&registry, Some(90), now).is_empty());
    }

    #[test]
    fn stale_check_skips_a_project_with_no_graph_to_date() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("undatable");
        std::fs::create_dir_all(&path).unwrap(); // exists, but no .infigraph/graph
        let registry = registry_with(vec![entry("undatable", path)]);

        let far_future = SystemTime::now() + Duration::from_secs(1000 * DAY_SECS);
        assert!(
            plan_registry_gc(&registry, Some(1), far_future).is_empty(),
            "a project whose last-index time is unknowable must never be age-evicted"
        );
    }

    #[test]
    fn execute_removes_evicted_repos_and_prunes_group_members() {
        let tmp = tempfile::tempdir().unwrap();
        let (live, _) = live_project(&tmp, "live");
        let mut registry = registry_with(vec![
            entry("live", live),
            entry("gone", tmp.path().join("does-not-exist")),
        ]);
        registry.groups.insert(
            "team".to_string(),
            Group {
                name: "team".to_string(),
                org: String::new(),
                repos: vec![
                    "live".to_string(),
                    "gone".to_string(),
                    "never-registered".to_string(),
                ],
                contracts: vec![],
            },
        );

        let plan = plan_registry_gc(&registry, None, SystemTime::now());
        assert_eq!(
            plan.dangling_group_members,
            vec![
                ("team".to_string(), "gone".to_string()),
                ("team".to_string(), "never-registered".to_string()),
            ],
            "both the freshly-evicted member and the already-dangling one must be pruned"
        );

        execute_registry_gc(&mut registry, &plan);

        assert!(!registry.repos.contains_key("gone"));
        assert!(registry.repos.contains_key("live"));
        assert_eq!(
            registry.groups["team"].repos,
            vec!["live".to_string()],
            "group must keep only members that still exist in the registry"
        );
    }

    #[test]
    fn empty_registry_yields_an_empty_plan() {
        let plan = plan_registry_gc(&Registry::default(), Some(1), SystemTime::now());
        assert!(plan.is_empty());
    }
}
