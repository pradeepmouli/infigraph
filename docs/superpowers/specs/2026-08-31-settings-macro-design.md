# Unified settings macro — design spec

## Problem

Every configurable behavior in infigraph today is wired up ad hoc, independently, at each point of use:

- **47 distinct `INFIGRAPH_*` env vars** are read via scattered `std::env::var("INFIGRAPH_...")` calls (a few via a named `..._ENV: &str` const, most via the literal string inline), each with its own hand-written parse/default/error-handling.
- A separate hand-rolled TOML mechanism (`crates/infigraph-core/src/watch/config.rs`) covers `[watch]`/`[watch_docs]` policy only.
- CLI flags exist only in `infigraph-cli` (which depends on `clap` with the `derive`+`env` features). `infigraph-mcp` has **no clap dependency at all** and hand-parses `std::env::args()`.
- The worst duplication: **`INFIGRAPH_BACKEND` is read independently at 15+ call sites** across `infigraph-core`, `infigraph-cli`, `infigraph-mcp`, and `infigraph-docs` (`core/lib.rs`, `core/daemon/lifecycle.rs`, `core/multi/mod.rs`, `cli/index.rs`, `cli/main.rs`, `cli/info_commands.rs`, `cli/group_commands.rs`, `mcp/health.rs`, `mcp/tools/{helpers,index,search,docs,watch,groups}.rs`, `docs/lib.rs`, `docs/search.rs`), each repeating its own `.unwrap_or_else(|| "kuzu".into())`-style default. This is a direct DRY violation, not just a style inconsistency — a future change to the default or the accepted value set has to be hunted down and changed in over a dozen places.

Full current inventory of the 47 vars: see the "Inventory" section below.

## Goals

- One convention-driven macro generates, for a settings group, a struct wired to CLI + env var + TOML, with **zero per-field naming attributes**.
- No new proc-macro/`syn`/`quote` dependency — a home-grown `macro_rules!`, matching this repo's existing preference for minimal-dependency, hand-rolled infrastructure (see `crates/infigraph-core/src/watch/config.rs`'s own hand-rolled TOML loader as precedent).
- Zero behavior change for existing env var **names** — this migration must not require anyone (users, CI configs, docs) to rename an env var they already set.
- CLI flags, env vars, and TOML sections must not collide across groups once multiple groups are combined into one binary's CLI surface.

## Convention

- A settings group is declared once, at a module's entry point, e.g.:
  ```rust
  settings! {
      struct Settings {
          idle_grace_secs: u64 = 300,
          idle_poll_secs: u64 = 5,
          lock_heartbeat_secs: u64 = 10,
          // ...
      }
  }
  ```
- **Group prefix** = the last segment of `module_path!()` at the macro's call site, with two adjustments:
  - If that last segment is literally `config` (e.g. a `watch/config.rs` submodule), use the segment above it instead — so both a flat `watch.rs` and a `watch/config.rs` layout produce prefix `watch`.
  - A leading `infigraph_` crate-root segment is stripped (e.g. `infigraph_mcp` → `mcp`).
- **Env var name**: `INFIGRAPH_{PREFIX}_{FIELD}` (upper-cased), e.g. `watch` + `idle_grace_secs` → `INFIGRAPH_WATCH_IDLE_GRACE_SECS`. Where a field already carries the group name for historical reasons (e.g. today's `INFIGRAPH_MCP_IDLE_GRACE_SECS`), the migrated field is named without the redundant prefix (`idle_grace_secs`) since the macro supplies `MCP_` from the module path — the generated env var name is unchanged.
- **TOML section**: nested under a section named after the prefix, via serde's ordinary field-name matching — no macro support needed for this part at all.
- **CLI flag**: see "CLI flag qualification" below.
- **`watch` vs `watch_docs`**: collapsed into one `watch` group, distinguished by field name (`watch_docs_enabled`, `watch_code_enabled`) rather than by two separate groups/prefixes.
- **Merge precedence**: CLI > env > TOML > hardcoded default, applied per field via `Option<T>` fields internally and a generated merge method chaining `Option::or()`.

## CLI flag qualification

Naively letting clap auto-derive `--long` from each field's bare name only works while a single settings group's flags are the only ones on a given command. Since multiple groups are commonly combined into one CLI surface (`#[command(flatten)]` is the standard clap pattern for composing structs), two groups both having a field like `enabled` would otherwise collide on `--enabled`.

Fix: the macro uses the `paste` crate (small, extremely common, near-zero-risk — used purely for its ident-pasting, not adopting it as an architecture) to generate the underlying struct's *field* as the already-prefixed identifier `{prefix}_{field}` (e.g. `watch_idle_grace_secs`), and lets clap's own standard field-name → kebab-case flag derivation do the rest for free, producing `--watch-idle-grace-secs`. `macro_rules!` alone cannot paste two tokens into a new identifier (no stable `concat_idents!`), which is why `paste` is needed — this is exactly the problem it exists to solve. No runtime string-building, no fighting clap's attribute parser (which wants a literal, not a computed value, for `#[arg(long = "...")]`).

This is a new dependency addition (`paste`) for `infigraph-core`, `infigraph-cli`, and (if adopting clap there — see Open Questions) `infigraph-mcp`.

## Generated shape (sketch, not final)

```rust
#[derive(Parser, Deserialize, Default)]
struct RawSettings {
    #[arg(long)] // field name is already prefixed via paste!, e.g. watch_idle_grace_secs
    watch_idle_grace_secs: Option<u64>,
    // ...
}

impl Settings {
    fn resolve() -> Self {
        let cli = RawSettings::parse();
        let env = RawSettings::from_env(); // per-field INFIGRAPH_{PREFIX}_{FIELD} lookup
        let toml = RawSettings::from_toml_section("watch");
        cli.merge(env).merge(toml).merge(Self::defaults())
    }
}
```
Exact generated method names/shapes (`resolve_env`, `merge_with`, etc.) are an implementation detail to finalize during the first spike, not part of this spec's contract.

## Inventory (47 settings, current state)

Grouped by natural target settings-group (inferred from name/usage — the actual group is whatever module the migrated `settings!` block ends up living in):

**`backend`** (read in 15+ separate production call sites — highest-value single migration): `INFIGRAPH_BACKEND`

**`mcp`** (idle/lock lifecycle, already named with redundant `MCP_` prefix): `INFIGRAPH_MCP_IDLE_GRACE_SECS`, `INFIGRAPH_MCP_IDLE_POLL_SECS`, `INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS`, `INFIGRAPH_MCP_LOCK_PATH`, `INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS`, `INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS`, `INFIGRAPH_MCP_LOCK_WEDGED_SECS`, `INFIGRAPH_MCP_LOG_PATH`, `INFIGRAPH_SUPERVISOR_PID` (`mcp/lifecycle.rs`), `INFIGRAPH_METRICS` (`mcp/lib.rs`)

**`watch`** (collapsed `watch`/`watch_docs`): `INFIGRAPH_NO_WATCH`, `INFIGRAPH_WATCH_DAEMON`, `INFIGRAPH_WATCH_ENABLED`, `INFIGRAPH_WATCH_DOCS_ENABLED`, `INFIGRAPH_DOC_DAEMON_POLL_MS`, `INFIGRAPH_INDEX_VIA_DAEMON`, `INFIGRAPH_AUTO_START_WATCH`, `INFIGRAPH_REAP_SCAN_SECS`, `INFIGRAPH_STORM_THRESHOLD`, `INFIGRAPH_TEST_DAEMON_PANIC` (test-only escape hatch — candidate to leave un-migrated, see Open Questions)

**`graph`**: `INFIGRAPH_GRAPH_GROWTH_MAX_RATIO`, `INFIGRAPH_QUARANTINE_MAX_BYTES`, `INFIGRAPH_SLOW_LOCK_MS`, `INFIGRAPH_DOC_HNSW_THRESHOLD`

**`registry`** (install/instance/registry plumbing): `INFIGRAPH_INSTALL_DIR`, `INFIGRAPH_BIN`, `INFIGRAPH_REGISTRY_HOME`, `INFIGRAPH_INSTANCES_DIR`, `INFIGRAPH_GH_HOST`, `INFIGRAPH_GH_OWNER`, `INFIGRAPH_ORG`

**`llm`** (review/Confluence LLM calls): `INFIGRAPH_LLM_BASE_URL`, `INFIGRAPH_LLM_EXTRACT`, `INFIGRAPH_LLM_MAX_TOKENS`, `INFIGRAPH_LLM_MODEL`, `INFIGRAPH_API_KEY`

**`session`** (MCP session/compression context): `INFIGRAPH_COMPRESSION_LEVEL`, `INFIGRAPH_ML_COMPRESSION`, `INFIGRAPH_DEDUP`, `INFIGRAPH_TOKEN_BUDGET`, `INFIGRAPH_MODEL_DIR`, `INFIGRAPH_KOMPRESS_DIR`

**Ungrouped / likely stay as raw env reads** (build-time constants, debug toggles, or genuinely test-only — see Open Questions): `INFIGRAPH_BUILD_HASH` (compile-time `env!`, not runtime), `INFIGRAPH_DEBUG`, `INFIGRAPH_DRIVER_JAR`, `INFIGRAPH_SCAN_ROOTS`

## Migration approach

Do **not** attempt all 47 in one PR. Sequence:

1. **Spike** the macro against the `mcp` idle/lock group (`crates/infigraph-mcp/src/idle.rs` + `mcp_lock.rs`) — smallest, cleanest existing worked example, validates the shape (including the `paste!`-based CLI qualification) end-to-end before committing further.
2. Migrate **`backend`** next — highest duplication payoff (15+ call sites collapse to one `Settings::backend()` accessor), and the biggest DRY win in the codebase per the user's global #1 rule.
3. Remaining groups (`watch`, `graph`, `registry`, `llm`, `session`) follow once the pattern is proven, each as its own PR.
4. Leave the "ungrouped" bucket as-is unless a specific need to formalize one arises later — not every env var needs to become a first-class "setting."

## Testing strategy

- Preserve the existing `ENV_LOCK`-style serialization pattern (`crates/infigraph-core/src/watch/config.rs:106`) for any test that mutates a macro-backed env var — `cargo test` runs unit tests as threads in one process, so concurrent env mutation across tests already requires this, and the macro doesn't change that constraint.
- Env var **names** must not change as part of migration — existing tests that `set_var`/`env_remove` a given `INFIGRAPH_*` name must keep working unmodified.
- Each migrated group needs a focused test asserting the full precedence chain (CLI > env > TOML > default) resolves correctly, plus a test that the group prefix derives correctly from `module_path!()` including the `config`-segment-skip rule.

## Resolved decisions

- **`infigraph-mcp` adopts `clap`.** It has no clap dependency today (hand-parses `std::env::args()` for `--mcp`/`--worker`), but that CLI surface is already tiny and fixed, so adding clap is low-cost. This keeps the `settings!` macro's generated code identical across every crate — no conditional/hand-rolled arg-scanner path, no special-casing for `mcp`-owned groups. Prerequisite for migrating any `mcp`-group setting that should also be CLI-settable: add `clap` (`derive`+`env` features, matching `infigraph-cli`'s existing dependency) to `infigraph-mcp`'s `Cargo.toml` before or alongside the first `mcp`-group migration.

## Open questions

- `INFIGRAPH_TEST_DAEMON_PANIC` and similar test-only escape hatches: migrate them into the macro (for consistency) or leave them as raw `std::env::var` reads (since they're intentionally undocumented and not meant to look like first-class settings)?
- `INFIGRAPH_BUILD_HASH` is read via the compile-time `env!()` macro, not `std::env::var` — out of scope for a runtime-settings macro entirely; listed here only for completeness of the inventory.
