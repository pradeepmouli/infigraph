# Refuse, don't wipe, a graph from a different lbug storage version (#140)

**Goal:** `Infigraph::init` must treat an embedded-DB storage-version mismatch as
"this build cannot read that graph", not as corruption. Refuse loudly, name both
versions, leave the file untouched. Closes the R3.1.1 gap the design doc already
lists ("lock contention, ENOSPC, EMFILE, permissions, **version mismatch** — are
surfaced as errors").

**Why now:** the `groups_watch_perf` stale-binary gotcha (a18f573) showed the
symptom end to end: a CLI on one lbug version indexes, a process on another opens
the graph, `init()` takes the corruption branch (retry → quarantine → wipe →
rebuild → `Ok(())`) and serves an empty graph as healthy.

## What lbug actually does (verified in lbug 0.19.1 sources)

- The data file starts with the magic bytes `LBUG` followed by the storage version
  as a little-endian `u64` (`DatabaseHeader::serialize`; the debugging-info keys
  are compiled out in release builds, so the layout is exactly 4 + 8 bytes).
- `Checkpointer::readCheckpoint` (run by `Database::new` on any non-empty file)
  calls `DatabaseHeader::deserialize` directly, which throws
  `Trying to read a database file with a different version. Database file version:
  {saved}, Current build storage version: {current}` when
  `canReadStorageVersion(saved)` is false. Newer-than-current always fails; 0.19.1
  can still read v40–v42 files (0.16.0 could only read v40).
- `DatabaseHeader::readDatabaseHeader` (used by the shadow file / disk-size paths)
  swallows that exception and reports "no header" — irrelevant for open, but it is
  why the message must be matched from `Database::new`'s error, not hunted later.
- The Rust crate exposes `kuzu::get_storage_version()` (crate `lbug`, aliased
  `kuzu` in this workspace), which the test uses to stamp `current + 1`.

## Tasks

### Task 1 — Regression test (RED)

`crates/infigraph-core/src/lib.rs`, tests module, next to
`init_quarantines_instead_of_deleting_on_persistent_corruption`:
`init_refuses_a_graph_from_a_different_storage_version_instead_of_wiping_it`.
Seeds a graph, rewrites bytes 4..12 of the header with `current + 1`, then asserts
`init()` errors naming both versions, the file is byte-identical, and no
`graph.corrupt.*` entry exists. Expected to fail today with `init()` returning
`Ok` after a wipe.

### Task 2 — Classifier + refusal (GREEN)

Same file, next to `is_lock_contention_error` / `is_transient_wal_open_race_error`:

- `is_storage_version_mismatch_error(&anyhow::Error) -> bool` matching lbug's
  "Trying to read a database file with a different version" text.
- `storage_version_mismatch_context(db_path) -> String` naming this build's
  `kuzu::get_storage_version()`, stating the file was left untouched, and giving
  the two remediations: run every process on the same installed build
  (`infigraph doctor` / `infigraph ps` show mixed builds), or `infigraph index
  --full` to rebuild on this build's version (local mode snapshots then wipes
  before `init()`, so that path does not hit the refusal).
- In `Infigraph::init`: a new match arm right after the lock-contention arm returns
  the error with that context, before the backoff loop. The backoff loop's
  `Err(e)` arm also short-circuits on it (same shape as its lock-contention exit),
  so a version error surfacing after a transient first failure still never reaches
  the wipe.
- Unit tests for the classifier: matches lbug's message, does not match the
  existing genuine-corruption fixture.

### Task 3 — Verification and docs

- `cargo test -p infigraph-core --lib init_` (all init recovery tests), then the
  crate's full suite serially (`env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND`),
  `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`.
- `docs/DESIGN-hardening.md`: add a Shipped bullet under R3.1.1 for the version
  mismatch case.
- Commit; leave #140 open for the user to close after landing (fork convention).

## Out of scope (filed separately)

`DocIndex::init` (`crates/infigraph-docs/src/lib.rs`) and `open_combined_graph`
(`crates/infigraph-core/src/multi/combined.rs`) still wipe on *any* open error,
including lock contention and version mismatch. Derived data, so not the same
severity, but the same R3.1.1 class — tracked in #143 rather than widened into
this fix.
