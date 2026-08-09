# Design: Git Worktree Lifecycle Hooks

**Date:** 2026-08-09
**Status:** Approved (brainstorming session)
**Scope:** Local (Kuzu/DaemonKuzu) mode only — see "Backend scope" below.

## Motivation

Infigraph has zero concept of git worktrees today: project identity is pure filesystem
path (`Registry` keyed by name/path/symbol_count, `Infigraph::open(root: &Path)`).
Confirmed empirically via `infigraph doctor --global`: a git worktree shows up as a
fully independent registered project, indistinguishable from any other directory.

Two consequences worth fixing:

1. **Cold-start cost.** A new worktree gets indexed from scratch — full parse,
   full cross-file resolution, full embedding — even though it usually shares the
   overwhelming majority of its files, byte-for-byte, with the repo's main worktree
   (that's the point of `git worktree`).
2. **Registry rot.** A removed worktree leaves a permanent, orphaned registry entry.
   Nothing evicts it (`R7.1`/[#12](https://github.com/pradeepmouli/infigraph/issues/12),
   registry GC, is still "Not started" in `DESIGN-hardening.md`).

This design adds worktree-aware bootstrap and teardown, triggered both by a Claude
Code hook (near-instant, for worktrees created/removed within an agent session) and
by a periodic/manual reconciliation sweep (the fallback for everything else).

## Backend scope

This feature is **local (Kuzu/DaemonKuzu) mode only**. Under the Neo4j/remote backend,
there is no per-project `.infigraph/` graph directory to clone — the graph is a shared,
multi-tenant store, and repo cardinality there is handled by `GraphBackend::repo_filter()`
namespace scoping, not by directory copying. That's a separate, already-orthogonal axis
(confirmed this session: storage backend and repo cardinality are independent concerns
in the existing code — `build_combined_graph` for local multi-repo groups is explicitly
a no-op under Neo4j, since Neo4j never needed a merge step). Worktree lifecycle hooks
under remote mode are out of scope for this design; if ever needed, they'd be a
different, much smaller feature (just registry bookkeeping, no clone).

## Command surface

### `infigraph clone <src-root> <dst-root>`

Standalone, general-purpose primitive — not worktree-specific. Copies
`<src-root>/.infigraph/` to `<dst-root>/.infigraph/`, **excluding**:
- Lock files: `graph.lock`, `watch.lock`, `mcp.lock`, `index.lock`. `LockInfo`
  (`crates/infigraph-core/src/lockfile.rs`) carries `pid`/`build_hash`/timestamps with
  no path field — a copied lock file wouldn't be wrong about a path, but it would
  falsely claim the destination's graph is held by the source's (possibly still-live)
  watcher PID, causing `Infigraph::init()` to refuse to open it
  (`is_lock_contention_error` path).
- Log files (`watch.log`, anything under `.infigraph/logs/`).

Requires `<src-root>/.infigraph/` to exist — clear error ("nothing to clone from") if
not. **Does not index.** Single responsibility: safely seed a new location from an
existing one. Correctness rests on a verified fact: `index_file`
(`crates/infigraph-core/src/lib.rs:700`) strips every path to project-root-relative
(`strip_prefix(&self.root)`) before it touches the graph — nothing in Symbol/File/Module
nodes encodes the absolute root path, so the copied data is valid at the new root as-is.

### `infigraph worktree init <path>`

One-shot bootstrap, called once per worktree at creation.

1. Resolve the repo's **main worktree**: the first entry of `git worktree list --porcelain`
   run from `<path>` — git always lists the main worktree first, regardless of which
   worktree you query from, so no branch-ancestry walking is needed. (Implementation
   note: confirm this ordering guarantee against real `git worktree list --porcelain`
   output at build time rather than trusting this doc alone — it is not yet verified
   against this repo's actual git version's output.)
2. If the main worktree has `.infigraph/`: `infigraph clone <main> <path>`, then
   `infigraph index <path>` (incremental — the existing per-file SHA-256 content-hash
   machinery on `Module` nodes does the reconciliation for free: files identical to the
   main worktree's checkout are skipped entirely, only files that actually differ on
   this branch get re-extracted).
3. If the main worktree has no `.infigraph/` (never indexed): plain from-scratch
   `infigraph index <path>`. Not an error — just the default path.

### `infigraph worktree teardown <path>`

One-shot cleanup, called once per worktree at removal.

1. Stop its watcher: `watch-stop <path>` (existing, already path-scoped).
2. Evict its registry entry: a new, narrow function extracted from the existing
   `Delete` command's deregister logic (which today also deletes `.infigraph/` data —
   more than approved here; this reuses only the registry-eviction half).
3. **Never touches `.infigraph/`.** Whether that directory still exists afterward
   depends entirely on how the worktree went away: `git worktree remove` deletes it as
   part of removing the worktree (nothing left to "leave on disk"); a manual `rm -rf`
   leaves nothing either. The "don't delete `.infigraph/`" rule mainly matters for the
   rare case where the directory persists but git no longer considers it a worktree.

### `infigraph worktree reconcile [--global]`

The repeatable sweep — the only one of the three verbs meant for ongoing/periodic use
(`init`/`teardown` each fire exactly once per worktree, at a specific lifecycle event;
naming them as one "sync" verb was considered and rejected as imprecise for that reason).

For each repo among the projects in scope (default: just the repo containing the
current working directory, resolved via `git rev-parse --git-common-dir`; every
registered project's repo under `--global`): diff `git worktree list --porcelain`
against registry entries whose path resolves, via that same `--git-common-dir` check,
to that same repo.

- **Teardown candidates** (registered, no longer in git's list): **acted on** —
  same low-risk, reversible action as `worktree teardown`, safe to batch.
- **Bootstrap candidates** (in git's list, no `.infigraph/` yet): **reported only**
  (e.g. "N unindexed worktrees found, run `infigraph worktree init <path>` for each").
  Auto-cloning and indexing an unknown number of worktrees during a sweep is exactly
  the kind of surprise action this repo's hardening work has been eliminating all
  week — indexing is comparatively expensive and sometimes simply not wanted yet.

### `doctor`'s new check

A new check function, same shape as the existing `check_one_sidecar`/
`check_project_registration` (`crates/infigraph-core/src/doctor.rs`): WARN with a
precise remediation naming the exact fix for that path — `infigraph worktree init
<path>` for a bootstrap candidate, `infigraph worktree teardown <path>` for a teardown
candidate — not a generic pointer at `reconcile`. Shares its detection logic with
`reconcile --global`'s report path (one function, two callers) rather than duplicating
the diff. `doctor` itself still mutates nothing — this is purely a new detection
surface, its read-only contract is unchanged.

### Claude Code PostToolUse hook

Matches Bash tool calls whose command contains `git worktree add`, `git worktree
remove`, or `git worktree prune`. On success, re-runs `git worktree list --porcelain`
to get the authoritative resulting path (safer than parsing the triggering command's
own arguments, which may use a default worktree name or relative path) and calls
`infigraph worktree init <path>` or `infigraph worktree teardown <path>` accordingly.

## Error handling

No new coordination logic for a torn `clone` (source's watcher mid-write at copy
time): `Infigraph::init()` already retries transient open failures with backoff and
quarantines-then-rebuilds on genuine corruption (R3.1.1/R3.1.2, shipped). Worst case
degrades to exactly the from-scratch index that would have happened without this
feature — never data loss, since `clone` only ever reads the source directory.

## Testing

- **`clone`:** copy round-trip (graph + sidecars present at destination); lock files
  and logs excluded; source directory untouched after clone.
- **`worktree init`:** real `git worktree add` in a temp repo with an indexed main
  worktree; assert the new worktree ends up correctly indexed; assert incremental
  reconciliation actually happened (e.g. `embeddings.bin` is *not* rewritten for
  symbols whose input text is identical between the two worktrees — direct reuse of
  this session's earlier embedding-hash-skip verification technique). Separate test:
  main worktree has no `.infigraph/` → falls back to plain full index.
- **`worktree teardown`:** registered project whose worktree path is then removed;
  assert registry eviction, assert watcher stopped, assert `.infigraph/` (when present)
  untouched.
- **`worktree reconcile --global`:** mixed drift across a couple of fake repos; assert
  the asymmetric behavior — teardown candidates acted on, bootstrap candidates only
  reported, never auto-indexed.
- **`doctor`'s new check:** mirrors `check_one_sidecar`'s existing unit-test shape.
- **Named gap:** the Claude Code PostToolUse hook itself is JSON config plus a shell
  invocation the harness drives — not exercisable by the Rust test suite. Flagged here
  as manual-verification-only rather than claimed as covered.

## Success criteria

- A worktree created within an agent session using `git worktree add` is indexed
  (via clone + incremental reindex, or full index if no seed exists) without a human
  running `infigraph index` manually.
- A worktree removed within an agent session has its registry entry evicted and its
  watcher stopped without a human running anything.
- `infigraph doctor --global` and `infigraph worktree reconcile --global` both
  correctly identify drift for worktrees created or removed outside any agent session
  (plain `git` in a terminal, another tool).
