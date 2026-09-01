# Remaining Settings Migration (install/registry/llm/web/session/embed) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate the last 18 inventory env vars (spec buckets `registry`, `llm`, `session`) onto `infigraph_core::settings!`, finishing the migration sequence.

**Architecture:** Six small groups, each declared in the module that owns the behavior (per the spec: "the actual group is whatever module the migrated `settings!` block ends up living in"): `install` (`core/lib.rs`), `registry` (`core/instances.rs`), `llm` (`core/review/llm.rs`), `embed` (`core/embed/mod.rs`), `web` (`mcp/web/mod.rs`), `session` (`mcp/session_context.rs`). Upstream-inherited names are preserved via the CLI-seed shim settled in the `graph` migration; a new `settings::legacy_env` helper makes that seed one line. Accessors use `RawXxx::default()` (never `parse_from`). Fields whose default is "computed at runtime" (home dirs) use `String` with empty = unset, so existing fallback logic and error behavior stay byte-identical.

**Tech Stack:** Rust, `infigraph_core::settings!`, `clap` (derive expands in the declaring crate only; consumers need no clap import).

## Global Constraints

- Provenance (`git log upstream/main -S<VAR>`, this session): fork-only → rename: `INFIGRAPH_INSTANCES_DIR` → `INFIGRAPH_REGISTRY_INSTANCES_DIR` (only one). Upstream-inherited → keep name, seed CLI slot: `INFIGRAPH_BIN`, `INFIGRAPH_GH_HOST`, `INFIGRAPH_GH_OWNER`, `INFIGRAPH_ORG`, `INFIGRAPH_API_KEY`, `INFIGRAPH_MODEL_DIR`, `INFIGRAPH_COMPRESSION_LEVEL`, `INFIGRAPH_ML_COMPRESSION`, `INFIGRAPH_DEDUP`, `INFIGRAPH_TOKEN_BUDGET`, `INFIGRAPH_KOMPRESS_DIR`. Already fit the convention (no shim): `INFIGRAPH_INSTALL_DIR`, `INFIGRAPH_REGISTRY_HOME`, `INFIGRAPH_LLM_{MODEL,BASE_URL,MAX_TOKENS,EXTRACT}`.
- Spec-bucket corrections (record in the spec): `INFIGRAPH_API_KEY` is the MCP HTTP-transport bearer key (`mcp/web/mod.rs::check_auth`), not an LLM key → `web { api_key }`. `INFIGRAPH_MODEL_DIR` is the embedding model dir (`core/embed`) → `embed { model_dir }`. `registry` bucket splits into `install { dir, bin, gh_host, gh_owner }` (self-update/install plumbing, DRY win: three duplicate `INFIGRAPH_GH_HOST`/`GH_OWNER` read sites in `cli/install.rs` collapse to one accessor) and `registry { home, instances_dir, org }`.
- `registry` is declared in `core/instances.rs`, not `core/multi/mod.rs`: `paste!` generates a `Registry` struct and `multi::Registry` (the registry data type) already exists there.
- Approved small behavior changes (the `Toggle` precedent from `watch`): `INFIGRAPH_DEDUP="false"` now means off (was: only `"0"`); `INFIGRAPH_LLM_EXTRACT="0"`/`"false"` now means off (was: any value enabled). `INFIGRAPH_API_KEY=""` now disables auth (was: required a `Bearer ` header with an empty key — functionally unauthenticated already).
- `install.sh`/`release.sh`/README keep using the legacy names — they stay valid via the shims; no script/doc churn.
- Per-crate verification: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p <crate> -- --test-threads=1`, one crate at a time. Rebuild `infigraph-cli` before `groups_watch_perf` if the debug binary is stale.

---

### Task 0: `settings::legacy_env` helper

**Files:** Modify `crates/infigraph-core/src/settings.rs` (after `env_override`); `crates/infigraph-docs/src/combined.rs` (use it).

- [ ] Add:

```rust
/// Reads a pre-macro env var by its legacy, non-convention name (e.g.
/// `INFIGRAPH_ORG`) for seeding into a settings group's CLI slot before
/// `resolve()`. The CLI slot outranks the macro's own env/TOML/default
/// layers, so the legacy name keeps winning without a second lookup
/// mechanism -- see `selected_backend()` for the precedent. Only for names
/// that exist upstream; fork-only names are renamed to the convention.
pub fn legacy_env<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}
```

- [ ] `combined.rs`: `graph_doc_hnsw_threshold: infigraph_core::settings::legacy_env("INFIGRAPH_DOC_HNSW_THRESHOLD"),`

### Task 1: `install` group (`core/lib.rs`, consumers `cli/install.rs`, `mcp/web/mod.rs`)

```rust
crate::settings! {
    install {
        dir: String = String::new(),
        bin: String = "/app/infigraph".to_string(),
        gh_host: String = "github.com".to_string(),
        gh_owner: String = "intuit".to_string(),
    }
}
pub fn install_settings() -> Install {
    let cli = RawInstall {
        install_bin: settings::legacy_env("INFIGRAPH_BIN"),
        install_gh_host: settings::legacy_env("INFIGRAPH_GH_HOST"),
        install_gh_owner: settings::legacy_env("INFIGRAPH_GH_OWNER"),
        ..Default::default()
    };
    Install::resolve(cli, None)
}
```
`cli/install.rs`: all three `gh_host`/`gh_owner` sites → `let install = infigraph_core::install_settings();`; `install_dir` = `install.dir` unless empty (then the existing `~/.local/bin` fallback). `mcp/web/mod.rs:82`: `let bin = infigraph_core::install_settings().bin;`.
Test `crates/infigraph-core/tests/install_settings.rs`: defaults; `INFIGRAPH_INSTALL_DIR` read; legacy `INFIGRAPH_GH_HOST` wins over canonical `INFIGRAPH_INSTALL_GH_HOST`.

### Task 2: `registry` group (`core/instances.rs`; consumers `core/multi/mod.rs`)

```rust
crate::settings! {
    registry {
        home: String = String::new(),
        instances_dir: String = String::new(),
        org: String = String::new(),
    }
}
pub fn registry_settings() -> Registry {
    let cli = RawRegistry { registry_org: crate::settings::legacy_env("INFIGRAPH_ORG"), ..Default::default() };
    Registry::resolve(cli, None)
}
```
`instances_dir()` reads `registry_settings().instances_dir` (empty → existing `$HOME/.infigraph/instances`). `multi::registry_path()` reads `.home` (empty → existing HOME/dirs logic, same error). `multi::default_org()` → `.org`. Rename `INFIGRAPH_INSTANCES_DIR` everywhere under `crates/` (`mcp/tests/{graceful_shutdown,instance_registration}.rs`, `core/tests/{instance_registry,doctor}.rs`, `core/src/ps.rs`, `core/src/instances.rs`).
Test `crates/infigraph-core/tests/registry_settings.rs`: `instances_dir()` honors the renamed var; `default_org()` legacy wins over canonical; `registry_path()` honors `INFIGRAPH_REGISTRY_HOME`.

### Task 3: `llm` group (`core/review/llm.rs`; consumer `confluence/template.rs`)

```rust
crate::settings! {
    llm {
        model: String = "claude-sonnet-4-20250514".to_string(),
        base_url: String = "https://api.anthropic.com".to_string(),
        max_tokens: u64 = 16384,
        extract: crate::settings::Toggle = crate::settings::Toggle(false),
    }
}
pub fn llm_settings() -> Llm { Llm::resolve(RawLlm::default(), None) }
```
`LlmConfig::from_env` and `template.rs::call_claude_extract` use `llm_settings()`; `fill_with_llm` gates on `.extract.0`.
Test `crates/infigraph-core/tests/llm_settings.rs`: defaults; `INFIGRAPH_LLM_MAX_TOKENS` read; `INFIGRAPH_LLM_EXTRACT=0` → false, `=1` → true.

### Task 4: `embed` group (`core/embed/mod.rs`)

```rust
crate::settings! { embed { model_dir: String = String::new(), } }
pub fn embed_settings() -> Embed {
    let cli = RawEmbed { embed_model_dir: crate::settings::legacy_env("INFIGRAPH_MODEL_DIR"), ..Default::default() };
    Embed::resolve(cli, None)
}
```
`find_model_dir` step 1 uses it (empty → skip). Test `crates/infigraph-core/tests/embed_settings.rs`: legacy vs canonical.

### Task 5: `web` group (`mcp/web/mod.rs`)

```rust
infigraph_core::settings! { web { api_key: String = String::new(), } }
fn api_key() -> Option<String> {
    let cli = RawWeb { web_api_key: infigraph_core::settings::legacy_env("INFIGRAPH_API_KEY"), ..Default::default() };
    Some(Web::resolve(cli, None).api_key).filter(|k| !k.is_empty())
}
```
`check_auth` uses `api_key()`.

### Task 6: `session` group (`mcp/session_context.rs`; consumer `mcp/compress.rs`)

Declared with real defaults for documentation; every consumer reads the CLI/env layer via `session_cli()` (legacy name, then canonical) so the existing config.toml layer keeps its place (the `auto_start_watch_on_boot_enabled` pattern). `SessionContext::new` (token budget), `get_ml_compression_mode`, `parse_level_override`, `apply_seen_dedup` (dedup → `Toggle`), and a new `kompress_dir_override()` for `compress.rs`. Existing session_context tests (legacy names) must pass unchanged.

### Task 7: Docs + full verification

Update the spec's Inventory bullets for `registry`/`llm`/`session` (done + corrections above) and Migration approach step 3 ("all groups migrated"). Then per-crate suites for core, docs, cli, mcp, confluence; `cargo fmt --all -- --check`; `cargo clippy --all-targets -- -D warnings`.
