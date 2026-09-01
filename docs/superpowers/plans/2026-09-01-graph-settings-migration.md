# Graph Settings Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate the `graph` settings group (4 env vars) onto `infigraph_core::settings!`, renaming its 3 fork-only vars to a consistent `INFIGRAPH_GRAPH_*` prefix and shimming the 1 upstream-inherited var so its existing name keeps working.

**Architecture:** The `graph { ... }` group is declared once in `crates/infigraph-core/src/graph/mod.rs` (its natural home, mirroring `watch/mod.rs`). Each of the 4 existing private accessor functions keeps its name and signature; only its body is rewired to resolve through `Graph::resolve(...)`. The upstream-inherited `INFIGRAPH_DOC_HNSW_THRESHOLD` is read by its legacy name and seeded into the generated `RawGraph`'s CLI slot — the exact precedent `infigraph_core::selected_backend()` already set for `INFIGRAPH_BACKEND` — so the macro's precedence chain still applies and the legacy name keeps winning.

**Tech Stack:** Rust, `macro_rules!` (`infigraph_core::settings!`), `clap`, `toml_edit`.

## Global Constraints

- Upstream provenance, verified via `git log upstream/main -S<VAR>` this session: `INFIGRAPH_QUARANTINE_MAX_BYTES` and `INFIGRAPH_SLOW_LOCK_MS` have zero history on `upstream/main` → fork-only → real rename (every occurrence in production code, tests, and *living* docs updates; dated plan/spec documents under `docs/superpowers/` are historical records and are NOT edited). `INFIGRAPH_GRAPH_GROWTH_MAX_RATIO` already fits `INFIGRAPH_{CATEGORY}_{FIELD}` with category `graph` + field `growth_max_ratio` → no rename. `INFIGRAPH_DOC_HNSW_THRESHOLD` exists on `upstream/main` (commit `6a444e5`, v2.6.0) → its name must keep working unchanged.
- Shim strategy for the inherited var (this settles the question left open after the `watch` group): read the legacy name directly and seed it into the CLI slot before `resolve()`, exactly like `selected_backend()` in `crates/infigraph-core/src/lib.rs:227`. Legacy name > new canonical name (`INFIGRAPH_GRAPH_DOC_HNSW_THRESHOLD`, which the macro's env layer also honors) > default. Do not invent a second mechanism.
- Preserve exact existing defaults: growth ratio `10`, quarantine cap `1024 * 1024 * 1024` bytes, slow-lock threshold `2000` ms (currently `Duration::from_secs(2)`), HNSW threshold `200_000`.
- Preserve accessor names/signatures (`graph_growth_max_ratio() -> u64`, `quarantine_max_bytes() -> u64`, `slow_wait_threshold() -> Duration`, `combined_hnsw_threshold() -> usize`) — callers are untouched. The macro's field types must implement `FromStr + FromTomlItem`; only `u64`, `String`, `Toggle` do today, so `slow_lock_ms` and `doc_hnsw_threshold` are `u64` fields converted at the accessor (`Duration::from_millis`, `as usize`), same as `watch`'s `doc_daemon_poll_ms`/`storm_threshold`.
- `infigraph-core` and `infigraph-docs` already depend on `clap` (`derive` feature) — no `Cargo.toml` changes.
- No struct-name collision: nothing named `Graph`/`RawGraph` exists in any crate (verified by regex search this session), so `paste!`'s `graph` → `Graph`/`RawGraph` is safe.
- `GRAPH_GROWTH_MAX_RATIO_ENV` stays as a `const` because `check_graph_growth_ratio`'s user-facing error message interpolates it; only `DEFAULT_GRAPH_GROWTH_MAX_RATIO` goes away (the default now lives in the `settings!` declaration).
- Tests that mutate these env vars keep their existing serialization (`ENV_LOCK` in `tests/quarantine_cap.rs`, `SLOW_LOCK_ENV` in `tests/lockfile.rs`, `COMBINED_DOCS_LOCK` in `infigraph-docs/tests/combined_docs.rs`).
- **Implementation note (found during execution):** the accessors build their CLI slot with `RawGraph::default()` (the generated struct derives `Default`; every slot `None`), not `RawGraph::parse_from(std::iter::empty())` as the code blocks below show. `slow_wait_threshold()` runs on every successful acquire *while the lock is held*, and constructing a clap `Command` there lengthened each hold enough to push contended waiters into `acquire()`'s 1ms→500ms backoff — `write_lock_perf::test_contended_lock_throughput` (pre-commit hook) failed 5/5 until this change. The two forms are otherwise equivalent; `default()` is the one to copy for future accessors.
- Verification per crate: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p <crate> -- --test-threads=1`, one crate at a time.

---

### Task 1: Declare the `graph` group and migrate `growth_max_ratio`

**Files:**
- Modify: `crates/infigraph-core/src/graph/mod.rs` (after the `pub use test_templates::...;` line, before `pub fn schema_ddl`)
- Modify: `crates/infigraph-core/src/graph/store_util.rs:53-68`
- Create: `crates/infigraph-core/tests/graph_settings.rs`

**Interfaces:**
- Produces: `infigraph_core::graph::{RawGraph, Graph}` with fields `growth_max_ratio: u64`, `quarantine_max_bytes: u64`, `slow_lock_ms: u64`, `doc_hnsw_threshold: u64`, and `Graph::resolve(cli: RawGraph, toml: Option<&toml_edit::Item>) -> Graph`. Tasks 2–4 consume these.

- [ ] **Step 1: Write the failing test**

Create `crates/infigraph-core/tests/graph_settings.rs`:

```rust
use clap::Parser;
use std::sync::Mutex;

// INFIGRAPH_GRAPH_* vars are process-wide; serialize tests that set them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn resolve_graph() -> infigraph_core::graph::Graph {
    let cli = infigraph_core::graph::RawGraph::parse_from(std::iter::empty::<String>());
    infigraph_core::graph::Graph::resolve(cli, None)
}

#[test]
fn graph_group_defaults_match_the_pre_migration_values() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for var in [
        "INFIGRAPH_GRAPH_GROWTH_MAX_RATIO",
        "INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES",
        "INFIGRAPH_GRAPH_SLOW_LOCK_MS",
        "INFIGRAPH_GRAPH_DOC_HNSW_THRESHOLD",
    ] {
        std::env::remove_var(var);
    }
    let g = resolve_graph();
    assert_eq!(g.growth_max_ratio, 10);
    assert_eq!(g.quarantine_max_bytes, 1024 * 1024 * 1024);
    assert_eq!(g.slow_lock_ms, 2000);
    assert_eq!(g.doc_hnsw_threshold, 200_000);
}

#[test]
fn growth_max_ratio_reads_its_env_var() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_GRAPH_GROWTH_MAX_RATIO", "3");
    assert_eq!(resolve_graph().growth_max_ratio, 3);
    std::env::remove_var("INFIGRAPH_GRAPH_GROWTH_MAX_RATIO");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test graph_settings -- --test-threads=1`
Expected: compile error — `no RawGraph in infigraph_core::graph`.

- [ ] **Step 3: Declare the group**

In `crates/infigraph-core/src/graph/mod.rs`, insert after `pub use test_templates::{test_templates_for, TestTemplate};`:

```rust
// Graph-store tunables. The `settings!` field pattern has no attribute
// capture, so per-field docs live on each accessor instead:
// - growth_max_ratio: runaway-growth circuit breaker
//   (`store_util::graph_growth_max_ratio`, #100)
// - quarantine_max_bytes: corrupt-base-image byte cap, 0 disables
//   (`quarantine::quarantine_max_bytes`, R7.3 / #100)
// - slow_lock_ms: slow-acquire recording threshold
//   (`lockfile::slow_wait_threshold`)
// - doc_hnsw_threshold: combined-docs HNSW build threshold; also readable
//   by its pre-macro upstream name `INFIGRAPH_DOC_HNSW_THRESHOLD`
//   (`infigraph-docs` `combined_hnsw_threshold`)
crate::settings! {
    graph {
        growth_max_ratio: u64 = 10,
        quarantine_max_bytes: u64 = 1024 * 1024 * 1024,
        slow_lock_ms: u64 = 2000,
        doc_hnsw_threshold: u64 = 200_000,
    }
}
```

(`///` doc comments on the fields would not parse: the macro's field pattern is `$field:ident : $ty:ty = $default:expr` with no `#[$meta]` capture. `//` comments are fine — they are not tokens.)

- [ ] **Step 4: Migrate `graph_growth_max_ratio`**

In `crates/infigraph-core/src/graph/store_util.rs`, add `use clap::Parser;` to the imports (after `use anyhow::Result;`), then replace lines 53–68 with:

```rust
/// Kept for `check_graph_growth_ratio`'s user-facing "override with ..."
/// hint; the value itself resolves through the `graph` settings group.
const GRAPH_GROWTH_MAX_RATIO_ENV: &str = "INFIGRAPH_GRAPH_GROWTH_MAX_RATIO";

/// Observed pathological incidents (github.com/pradeepmouli/infigraph#100)
/// were 40-70x a healthy graph's size; the default 10x gives wide headroom
/// for legitimate growth (large refactors, new language support landing)
/// while still catching the actual pattern well before it reaches
/// disk-filling scale. Resolved via the `graph` settings group
/// (`INFIGRAPH_GRAPH_GROWTH_MAX_RATIO`).
fn graph_growth_max_ratio() -> u64 {
    let cli = crate::graph::RawGraph::parse_from(std::iter::empty::<String>());
    crate::graph::Graph::resolve(cli, None).growth_max_ratio
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test graph_settings -- --test-threads=1`
Expected: 2 passed. Also run `cargo test -p infigraph-core --lib store_util` → all existing growth-ratio unit tests still pass.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/graph/mod.rs crates/infigraph-core/src/graph/store_util.rs crates/infigraph-core/tests/graph_settings.rs
git commit -m "feat(core): add graph settings group, migrate growth_max_ratio"
```

---

### Task 2: Migrate `quarantine_max_bytes` (rename `INFIGRAPH_QUARANTINE_MAX_BYTES` → `INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES`)

**Files:**
- Modify: `crates/infigraph-core/src/quarantine.rs:1-2` (imports), `:34-40` (accessor), `:122` (manifest reason string)
- Modify: `crates/infigraph-core/src/graph/store_util.rs` (doc comment mentioning the old name, in the block Task 1 rewrote — verify none remains)
- Modify: `crates/infigraph-core/tests/quarantine_cap.rs:11,17,22`
- Modify: `docs/DESIGN-hardening.md:36` (living doc)

**Interfaces:**
- Consumes: `crate::graph::{RawGraph, Graph}` from Task 1.

- [ ] **Step 1: Rename the env var in the existing tests (this is the failing test)**

In `crates/infigraph-core/tests/quarantine_cap.rs`, replace every `INFIGRAPH_QUARANTINE_MAX_BYTES` with `INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES` (3 occurrences: the doc comment on line 11, `set_var` on 17, `remove_var` on 22).

- [ ] **Step 2: Run to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test quarantine_cap -- --test-threads=1`
Expected: `oversized_corrupt_base_is_dropped_but_wal_and_manifest_survive` FAILS (the 16-byte cap set under the new name is ignored; the 64-byte base is kept).

- [ ] **Step 3: Migrate the accessor**

In `crates/infigraph-core/src/quarantine.rs`, add `use clap::Parser;` after `use anyhow::{Context, Result};`, then replace lines 34–40 with:

```rust
/// Resolved via the `graph` settings group
/// (`INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES`; 0 disables the cap).
fn quarantine_max_bytes() -> u64 {
    let cli = crate::graph::RawGraph::parse_from(std::iter::empty::<String>());
    crate::graph::Graph::resolve(cli, None).quarantine_max_bytes
}
```

And on line 122 change the manifest reason string to
`"corrupt base image exceeded INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES; WAL-family siblings retained for forensics"`
(also collapsing the accidental run of spaces in the existing literal).

- [ ] **Step 4: Update remaining references to the old name**

- `crates/infigraph-core/src/graph/store_util.rs`: Task 1's rewrite already dropped the old-name mention; confirm with `mcp__infigraph__search_code` pattern `INFIGRAPH_QUARANTINE_MAX_BYTES` over `crates/` — expected: zero matches after this step.
- `docs/DESIGN-hardening.md` line 36: replace `INFIGRAPH_QUARANTINE_MAX_BYTES` with `INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES`.
- Leave `docs/superpowers/plans/2026-08-24-*.md` and `docs/superpowers/specs/2026-08-24-*.md` untouched (historical).

- [ ] **Step 5: Run to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test quarantine_cap -- --test-threads=1`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/infigraph-core/src/quarantine.rs crates/infigraph-core/tests/quarantine_cap.rs docs/DESIGN-hardening.md
git commit -m "feat(core): migrate quarantine_max_bytes onto graph settings group"
```

---

### Task 3: Migrate `slow_wait_threshold` (rename `INFIGRAPH_SLOW_LOCK_MS` → `INFIGRAPH_GRAPH_SLOW_LOCK_MS`)

**Files:**
- Modify: `crates/infigraph-core/src/lockfile.rs:18` (imports), `:153-161` (accessor)
- Modify: `crates/infigraph-core/tests/lockfile.rs:8,207,235`

**Interfaces:**
- Consumes: `crate::graph::{RawGraph, Graph}` from Task 1.

- [ ] **Step 1: Rename the env var in the existing test (this is the failing test)**

In `crates/infigraph-core/tests/lockfile.rs`, replace every `INFIGRAPH_SLOW_LOCK_MS` with `INFIGRAPH_GRAPH_SLOW_LOCK_MS` (doc comment line 8, `set_var` line 207, `remove_var` line 235).

- [ ] **Step 2: Run to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test lockfile test_slow_wait_recorded_and_drained -- --test-threads=1`
Expected: FAIL — with the 50ms override no longer read, the threshold stays at 2s and the ~200ms wait is never recorded.

- [ ] **Step 3: Migrate the accessor**

In `crates/infigraph-core/src/lockfile.rs`, add `use clap::Parser;` after `use anyhow::Result;`, then replace lines 153–161 with:

```rust
/// Threshold above which a successful-but-slow acquisition is recorded.
/// Resolved via the `graph` settings group (`INFIGRAPH_GRAPH_SLOW_LOCK_MS`,
/// milliseconds; tests lower it).
pub fn slow_wait_threshold() -> Duration {
    let cli = crate::graph::RawGraph::parse_from(std::iter::empty::<String>());
    Duration::from_millis(crate::graph::Graph::resolve(cli, None).slow_lock_ms)
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test lockfile -- --test-threads=1`
Expected: all pass. Confirm zero remaining `INFIGRAPH_SLOW_LOCK_MS` under `crates/` via `mcp__infigraph__search_code`.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/lockfile.rs crates/infigraph-core/tests/lockfile.rs
git commit -m "feat(core): migrate slow_wait_threshold onto graph settings group"
```

---

### Task 4: Migrate `combined_hnsw_threshold` (keep `INFIGRAPH_DOC_HNSW_THRESHOLD` via CLI-seed shim)

**Files:**
- Modify: `crates/infigraph-docs/src/combined.rs:8` (imports), `:316-321` (accessor), `:657+` (tests module)

**Interfaces:**
- Consumes: `infigraph_core::graph::{RawGraph, Graph}` from Task 1.

- [ ] **Step 1: Write the failing test**

Inside the existing `#[cfg(test)] mod tests` block at the end of `crates/infigraph-docs/src/combined.rs`, add:

```rust
    /// `INFIGRAPH_DOC_HNSW_THRESHOLD` predates the settings macro and is
    /// upstream-inherited, so its name must keep working; the macro's own
    /// canonical name works too, and the legacy name wins when both are set
    /// (it is seeded into the CLI slot, which outranks env).
    #[test]
    fn hnsw_threshold_honors_legacy_name_over_canonical_name() {
        std::env::remove_var("INFIGRAPH_DOC_HNSW_THRESHOLD");
        std::env::remove_var("INFIGRAPH_GRAPH_DOC_HNSW_THRESHOLD");
        assert_eq!(combined_hnsw_threshold(), 200_000);

        std::env::set_var("INFIGRAPH_GRAPH_DOC_HNSW_THRESHOLD", "7");
        assert_eq!(combined_hnsw_threshold(), 7);

        std::env::set_var("INFIGRAPH_DOC_HNSW_THRESHOLD", "3");
        assert_eq!(combined_hnsw_threshold(), 3, "legacy name must win");

        std::env::remove_var("INFIGRAPH_DOC_HNSW_THRESHOLD");
        std::env::remove_var("INFIGRAPH_GRAPH_DOC_HNSW_THRESHOLD");
    }
```

(This is the only test in that module touching these vars, and `infigraph-docs/tests/combined_docs.rs` runs in a separate test binary/process, so no lock is needed here.)

- [ ] **Step 2: Run to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-docs --lib hnsw_threshold_honors_legacy_name_over_canonical_name`
Expected: FAIL at the second assertion (canonical name `INFIGRAPH_GRAPH_DOC_HNSW_THRESHOLD` is not read yet → 200_000 ≠ 7).

- [ ] **Step 3: Migrate the accessor**

In `crates/infigraph-docs/src/combined.rs`, add `use clap::Parser;` after `use anyhow::{Context, Result};`, then replace lines 316–321 with:

```rust
/// Embedding count above which the combined store builds an HNSW index.
/// Resolved via core's `graph` settings group. `INFIGRAPH_DOC_HNSW_THRESHOLD`
/// predates the macro (and exists upstream), so it is read by its legacy
/// name and seeded into the CLI slot -- which still outranks the macro's
/// own env/TOML/default layers -- exactly as `selected_backend()` does for
/// `INFIGRAPH_BACKEND`.
fn combined_hnsw_threshold() -> usize {
    let mut cli = infigraph_core::graph::RawGraph::parse_from(std::iter::empty::<String>());
    cli.graph_doc_hnsw_threshold = cli
        .graph_doc_hnsw_threshold
        .or_else(|| std::env::var("INFIGRAPH_DOC_HNSW_THRESHOLD").ok()?.parse().ok());
    infigraph_core::graph::Graph::resolve(cli, None).doc_hnsw_threshold as usize
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-docs -- --test-threads=1`
Expected: all pass, including the untouched `tests/combined_docs.rs` tests that set the legacy name.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-docs/src/combined.rs
git commit -m "feat(docs): migrate combined_hnsw_threshold onto core's graph settings group"
```

---

### Task 5: Docs and full verification

**Files:**
- Modify: `docs/superpowers/specs/2026-08-31-settings-macro-design.md` (Inventory `graph` bullet, line 106; Migration approach step 3, line 122)

- [ ] **Step 1: Update the spec**

Replace the Inventory `graph` bullet with:

```
**`graph`** — done (`docs/superpowers/plans/2026-09-01-graph-settings-migration.md`). Declared in `crates/infigraph-core/src/graph/mod.rs`. Renamed (fork-only, confirmed via `git log upstream/main -S`): `INFIGRAPH_QUARANTINE_MAX_BYTES` → `INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES`, `INFIGRAPH_SLOW_LOCK_MS` → `INFIGRAPH_GRAPH_SLOW_LOCK_MS`. Unchanged: `INFIGRAPH_GRAPH_GROWTH_MAX_RATIO` (already fit the convention). Shimmed (upstream-inherited, name preserved): `INFIGRAPH_DOC_HNSW_THRESHOLD`, read by its legacy name and seeded into the CLI slot exactly like `INFIGRAPH_BACKEND`; the canonical `INFIGRAPH_GRAPH_DOC_HNSW_THRESHOLD` also works, legacy wins.
```

In Migration approach step 3, change "Remaining groups (`graph`, `registry`, `llm`, `session`)" to "Remaining groups (`registry`, `llm`, `session`)" and append: "`graph` — done; it also settled the inherited-var shim strategy: read the legacy name, seed the CLI slot (the `INFIGRAPH_BACKEND` precedent), never a second mechanism."

- [ ] **Step 2: Full verification**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core -- --test-threads=1
env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-docs -- --test-threads=1
env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-cli -- --test-threads=1
env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-mcp -- --test-threads=1
```
Expected: all green (modulo the known pre-existing `write_lock.rs` failures documented in the `watch` plan's session notes — confirm any failure is pre-existing at the pre-plan commit before treating it as a regression).

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-08-31-settings-macro-design.md
git commit -m "docs(specs): mark the graph settings migration done"
```
