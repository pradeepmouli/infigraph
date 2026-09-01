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

- A settings group is declared once, at a module's entry point, as a single identifier — its **category** — followed by the field list:
  ```rust
  settings! {
      mcp_idle {
          grace_secs: u64 = 300,
          poll_secs: u64 = 5,
      }
  }
  settings! {
      mcp_lock {
          heartbeat_secs: u64 = 10,
          // ...
      }
  }
  ```
- **One identifier, four roles.** `$category` (e.g. `mcp_idle`) names the group's env var prefix (`INFIGRAPH_{CATEGORY}_{FIELD}`), its category-qualified CLI flags (via `paste!`), its TOML section, *and* the generated struct's name — via `paste!`'s `:camel` case conversion (`mcp_idle` → `McpIdle`/`RawMcpIdle`). Folding all four into one token (rather than a separate `category: X, struct Y` pair) means two settings groups that share a common namespace — `mcp_idle` and `mcp_lock`, both logically "under mcp" — stay distinct simply by being different identifiers, with no extra struct-name token to keep in sync. Verified directly: `crates/infigraph-core/src/settings.rs`'s own test suite declares `toy_a`/`toy_b` groups side by side and confirms they resolve independently with no collision.
- **Why an explicit `category` token, not silent `module_path!()` inference:** `paste!` (needed for CLI flag qualification — see below) can only paste together *tokens* available to the macro at compile time. `module_path!()`'s value is a string literal that's only splittable (extract last segment, apply the `config`-skip/`infigraph_`-strip rules) with ordinary runtime Rust code — `macro_rules!`/`paste!` cannot parse or re-tokenize it without a proc-macro (`syn`/`quote`), which this design explicitly avoids. So the category can't be silently inferred purely from file location while also driving `paste!`-based CLI flags and the generated struct name.
- **Env var name**: `INFIGRAPH_{CATEGORY}_{FIELD}` (upper-cased), e.g. category `mcp_idle` + field `grace_secs` → `INFIGRAPH_MCP_IDLE_GRACE_SECS`. Since env var construction is plain string concatenation, this is byte-identical to what a two-token `category: mcp` + field `idle_grace_secs` would have produced — folding the group's own name into the category costs nothing here, it just relocates which token contributes the `IDLE_`/`LOCK_` fragment.
- **TOML section**: nested under a section named after the category, via serde's ordinary field-name matching — no macro support needed for this part at all.
- **CLI flag**: see "CLI flag qualification" below.
- **`watch` vs `watch_docs`**: collapsed into one `watch` category, distinguished by field name (`watch_docs_enabled`, `watch_code_enabled`) rather than by two separate groups/categories.
- **Merge precedence**: CLI > env > TOML > hardcoded default, applied per field via `Option<T>` fields internally and a generated merge method chaining `Option::or()`.

## CLI flag qualification

Naively letting clap auto-derive `--long` from each field's bare name only works while a single settings group's flags are the only ones on a given command. Since multiple groups are commonly combined into one CLI surface (`#[command(flatten)]` is the standard clap pattern for composing structs), two groups both having a field like `enabled` would otherwise collide on `--enabled`.

Fix: the macro uses the `paste` crate (small, extremely common, near-zero-risk — used purely for its ident-pasting, not adopting it as an architecture) to generate the underlying struct's *field* as the already-qualified identifier `{category}_{field}` (e.g. `mcp_idle_grace_secs`), using the `$category` token from the macro invocation, and lets clap's own standard field-name → kebab-case flag derivation do the rest for free, producing `--mcp-idle-grace-secs`. `macro_rules!` alone cannot paste two tokens into a new identifier (no stable `concat_idents!`), which is why `paste` is needed — this is exactly the problem it exists to solve. No runtime string-building, no fighting clap's attribute parser (which wants a literal, not a computed value, for `#[arg(long = "...")]`).

This is a new dependency addition (`paste`) for `infigraph-core` (re-exported — see below) and `clap` for `infigraph-core` (dev-only, for the macro's own tests) plus every crate that declares a settings group (`infigraph-cli` already had it; `infigraph-mcp` gained it — see Resolved decisions).

`toml_edit::Item` (the type `resolve()`'s TOML parameter uses) is likewise a literal path in the generated code, so `infigraph-core` also re-exports `toml_edit` (`pub use toml_edit;`) and the macro references it as `$crate::toml_edit::Item` — otherwise every settings!-consuming crate would need its own direct `toml_edit` dependency just to satisfy that one signature. Same reasoning as the `paste` re-export; discovered when migrating `idle.rs`, the macro's first real cross-crate consumer.

## Generated shape (implemented)

```rust
settings! {
    mcp_idle {
        grace_secs: u64 = 300,
        poll_secs: u64 = 5,
    }
}
```
expands to (via `paste!`'s `:camel` case conversion of `mcp_idle` → `McpIdle`):
```rust
#[derive(Debug, Clone, Default, clap::Parser, serde::Deserialize)]
pub struct RawMcpIdle {
    #[arg(long)]
    pub mcp_idle_grace_secs: Option<u64>,
    #[arg(long)]
    pub mcp_idle_poll_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpIdle {
    pub grace_secs: u64,
    pub poll_secs: u64,
}

impl McpIdle {
    pub fn resolve(cli: RawMcpIdle, toml_section: Option<&toml_edit::Item>) -> Self {
        Self {
            grace_secs: cli.mcp_idle_grace_secs
                .or_else(|| infigraph_core::settings::env_override("mcp_idle", "grace_secs"))
                .or_else(|| toml_section.and_then(|s| s.get("grace_secs")).and_then(FromTomlItem::from_toml_item))
                .unwrap_or(300),
            poll_secs: /* same shape */ 5,
        }
    }
}
```
See `crates/infigraph-core/src/settings.rs` for the exact macro source and its own test suite (including the `toy_a`/`toy_b` no-collision proof).

## Inventory (47 settings, current state)

Grouped by natural target settings-group (inferred from name/usage — the actual group is whatever module the migrated `settings!` block ends up living in):

**`backend`** (read in 15+ separate production call sites — highest-value single migration): `INFIGRAPH_BACKEND`

**`mcp`** (idle/lock lifecycle, already named with redundant `MCP_` prefix): `INFIGRAPH_MCP_IDLE_GRACE_SECS`, `INFIGRAPH_MCP_IDLE_POLL_SECS`, `INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS`, `INFIGRAPH_MCP_LOCK_PATH`, `INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS`, `INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS`, `INFIGRAPH_MCP_LOCK_WEDGED_SECS`, `INFIGRAPH_MCP_LOG_PATH`, `INFIGRAPH_SUPERVISOR_PID` (`mcp/lifecycle.rs`), `INFIGRAPH_METRICS` (`mcp/lib.rs`)

**`watch`** — done. Migrated (renamed to fit the category): `INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS`, `INFIGRAPH_WATCH_INDEX_VIA_DAEMON`, `INFIGRAPH_WATCH_AUTO_START`, `INFIGRAPH_WATCH_REAP_SCAN_SECS`, `INFIGRAPH_WATCH_STORM_THRESHOLD`. Left unmigrated (documented reasons in `docs/superpowers/plans/2026-09-01-watch-settings-migration.md`): `INFIGRAPH_NO_WATCH`, `INFIGRAPH_WATCH_ENABLED`, `INFIGRAPH_WATCH_DOCS_ENABLED`, `INFIGRAPH_TEST_DAEMON_PANIC`. `INFIGRAPH_WATCH_DAEMON` was found to be dead code (no production reads) and removed from this inventory.

**`graph`**: `INFIGRAPH_GRAPH_GROWTH_MAX_RATIO`, `INFIGRAPH_QUARANTINE_MAX_BYTES`, `INFIGRAPH_SLOW_LOCK_MS`, `INFIGRAPH_DOC_HNSW_THRESHOLD`

**`registry`** (install/instance/registry plumbing): `INFIGRAPH_INSTALL_DIR`, `INFIGRAPH_BIN`, `INFIGRAPH_REGISTRY_HOME`, `INFIGRAPH_INSTANCES_DIR`, `INFIGRAPH_GH_HOST`, `INFIGRAPH_GH_OWNER`, `INFIGRAPH_ORG`

**`llm`** (review/Confluence LLM calls): `INFIGRAPH_LLM_BASE_URL`, `INFIGRAPH_LLM_EXTRACT`, `INFIGRAPH_LLM_MAX_TOKENS`, `INFIGRAPH_LLM_MODEL`, `INFIGRAPH_API_KEY`

**`session`** (MCP session/compression context): `INFIGRAPH_COMPRESSION_LEVEL`, `INFIGRAPH_ML_COMPRESSION`, `INFIGRAPH_DEDUP`, `INFIGRAPH_TOKEN_BUDGET`, `INFIGRAPH_MODEL_DIR`, `INFIGRAPH_KOMPRESS_DIR`

**Ungrouped / likely stay as raw env reads** (build-time constants, debug toggles, or genuinely test-only — see Open Questions): `INFIGRAPH_BUILD_HASH` (compile-time `env!`, not runtime), `INFIGRAPH_DEBUG`, `INFIGRAPH_DRIVER_JAR`, `INFIGRAPH_SCAN_ROOTS`

## Migration approach

Do **not** attempt all 47 in one PR. Sequence:

1. **Spike** the macro against the `mcp` idle/lock group (`crates/infigraph-mcp/src/idle.rs` + `mcp_lock.rs`) — smallest, cleanest existing worked example, validates the shape (including the `paste!`-based CLI qualification) end-to-end before committing further. — done (`docs/superpowers/plans/2026-08-31-settings-macro-mcp-spike.md`)
2. Migrate **`backend`** next — highest duplication payoff (15+ call sites collapse to one `Settings::backend()` accessor), and the biggest DRY win in the codebase per the user's global #1 rule. — done (`docs/superpowers/plans/2026-08-31-backend-settings-migration.md`). Also consolidated the ~10 duplicate `is_remote_mode()`/`is_neo4j_backend()` helpers found still live across infigraph-mcp/infigraph-cli/infigraph-docs/infigraph-core onto the pre-existing (but previously unused-for-this-purpose) `daemon::lifecycle::is_remote_backend()`.
3. Remaining groups (`graph`, `registry`, `llm`, `session`) follow once the pattern is proven, each as its own PR. `watch` — done (`docs/superpowers/plans/2026-09-01-watch-settings-migration.md`); its 5 real fields were all fork-only (confirmed via upstream content search) and got renamed to a consistent `INFIGRAPH_WATCH_*` prefix rather than shimmed, and two inconsistent boolean-truthy conventions were unified via a new reusable `Toggle` field type. `INFIGRAPH_WATCH_DAEMON` turned out to be dead code (removed from this bullet); `INFIGRAPH_WATCH_ENABLED`/`INFIGRAPH_WATCH_DOCS_ENABLED`/`INFIGRAPH_NO_WATCH`/`INFIGRAPH_TEST_DAEMON_PANIC` stay unmigrated (documented reasons in that plan's Global Constraints).
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
