# Settings Macro — MCP Idle/Lock Spike Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `settings!` macro (CLI + env var + TOML, zero per-field naming attributes) and migrate the `mcp` idle/lock settings group onto it, proving the shape end-to-end before any wider migration.

**Architecture:** A `macro_rules!` macro in `infigraph-core` generates, per settings group, a `Raw{Name}` struct (`Option<T>` fields, category-qualified via `paste!` so clap's own kebab-casing produces `--{category}-{field}` flags for free) and a resolved `{Name}` struct with a `resolve(cli, toml_section)` merging CLI > env > TOML > hardcoded default per field. `idle.rs` and `mcp_lock.rs` migrate their 6 numeric/duration settings onto it, keeping every existing public function name/signature unchanged so their existing tests pass unmodified.

**Tech Stack:** Rust, `clap` (`derive` feature), `paste`, `toml_edit` (already a dependency) — no new proc-macro/`syn`/`quote` dependency.

**Spec:** `docs/superpowers/specs/2026-08-31-settings-macro-design.md`

## Global Constraints

- Env var **names** must not change: `INFIGRAPH_MCP_IDLE_GRACE_SECS`, `INFIGRAPH_MCP_IDLE_POLL_SECS`, `INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS`, `INFIGRAPH_MCP_LOCK_WEDGED_SECS`, `INFIGRAPH_MCP_LOCK_TAKEOVER_POLL_SECS`, `INFIGRAPH_MCP_LOCK_TAKEOVER_TIMEOUT_SECS` all keep their exact current names.
- Every existing public function this plan touches (`idle_grace_period`, `idle_poll_interval`, `heartbeat_interval`, `wedged_threshold_secs`, `takeover_poll_interval`, `takeover_wait_timeout`) keeps its exact current name and signature — callers elsewhere in the codebase must not need to change.
- `crates/infigraph-mcp/src/mcp_lock.rs::lock_path` is explicitly **out of scope** for this spike (its default is computed from `$HOME`, not a literal — the macro's `= $default:expr` slot doesn't cover that case yet; revisit once a second computed-default setting justifies extending the macro, per YAGNI). Leave it untouched.
- No test may be modified to make it pass — `crates/infigraph-mcp/tests/idle_decision.rs` and `crates/infigraph-mcp/tests/mcp_lock.rs` must pass exactly as they are today, unedited, proving zero behavior change.
- Match this Mac's disk-constrained build workflow: run `cargo test` scoped per-crate (`-p infigraph-core -p infigraph-mcp`), never a bare `--all`.
- `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings` must pass before the final commit — this repo's pre-commit hook enforces both workspace-wide.

---

### Task 1: `settings!` macro and its own test coverage

**Files:**
- Create: `crates/infigraph-core/src/settings.rs`
- Modify: `crates/infigraph-core/src/lib.rs` (add `pub mod settings;` and `pub use paste;`)
- Modify: `crates/infigraph-core/Cargo.toml` (add `paste` as a regular dependency, `clap` as a dev-dependency)

**Interfaces:**
- Produces: `infigraph_core::settings::env_override<T: FromStr>(category: &str, field: &str) -> Option<T>`, `infigraph_core::settings::FromTomlItem` trait (with a `u64` impl), and the `infigraph_core::settings!` macro (`#[macro_export]`, invoked as `category: <ident>, struct <Name> { <field>: <Ty> = <default>, ... }`, generating `Raw<Name>` and `<Name>` with `<Name>::resolve(cli: Raw<Name>, toml_section: Option<&toml_edit::Item>) -> <Name>`).

- [ ] **Step 1: Add the new dependencies**

Edit `crates/infigraph-core/Cargo.toml`. In the `[dependencies]` section, add:

```toml
paste = "1"
```

In a new `[dev-dependencies]` entry (add the section if it doesn't already have one — check first, since `infigraph-core` already has a `[dev-dependencies]` section further down the file):

```toml
clap = { version = "4", features = ["derive"] }
```

- [ ] **Step 2: Write the macro and its runtime helpers**

Create `crates/infigraph-core/src/settings.rs`:

```rust
//! Convention-driven settings groups: one macro wires a struct to CLI
//! (clap), env vars, and TOML config with zero per-field naming attributes.
//! See docs/superpowers/specs/2026-08-31-settings-macro-design.md.

/// Reads `INFIGRAPH_{CATEGORY}_{FIELD}` (both upper-cased) and parses it.
/// Returns `None` if unset or unparseable -- the caller falls through to
/// the next precedence layer (TOML, then hardcoded default) in that case.
pub fn env_override<T: std::str::FromStr>(category: &str, field: &str) -> Option<T> {
    let key = format!(
        "INFIGRAPH_{}_{}",
        category.to_ascii_uppercase(),
        field.to_ascii_uppercase()
    );
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

/// Reads a single field out of a `toml_edit` section by name. Implemented
/// per concrete type actually used by a settings group -- add an impl the
/// first time a group needs a new field type, rather than speculatively
/// covering every possible type up front.
pub trait FromTomlItem: Sized {
    fn from_toml_item(item: &toml_edit::Item) -> Option<Self>;
}

impl FromTomlItem for u64 {
    fn from_toml_item(item: &toml_edit::Item) -> Option<Self> {
        item.as_integer().and_then(|i| u64::try_from(i).ok())
    }
}

/// Declares a settings group. `category` names the group for env var names
/// (`INFIGRAPH_{CATEGORY}_{FIELD}`), category-qualified CLI flags (via
/// `paste!`, so `--{category}-{field}` falls out of clap's own kebab-case
/// derivation with zero explicit `long = "..."` attributes), and the TOML
/// section it reads from. `category` is an explicit token rather than
/// derived from `module_path!()` because `paste!` can only paste compile-time
/// tokens, and `module_path!()`'s value is a runtime string macro_rules!
/// cannot re-tokenize without a proc-macro -- see the spec's "Convention"
/// section for the full reasoning.
#[macro_export]
macro_rules! settings {
    (
        category: $category:ident,
        struct $name:ident {
            $( $field:ident : $ty:ty = $default:expr ),+ $(,)?
        }
    ) => {
        $crate::paste::paste! {
            #[derive(Debug, Clone, Default, clap::Parser, serde::Deserialize)]
            pub struct [<Raw $name>] {
                $(
                    #[arg(long)]
                    pub [<$category _ $field>]: Option<$ty>,
                )+
            }
        }

        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            $( pub $field: $ty, )+
        }

        $crate::paste::paste! {
            impl $name {
                /// Resolves this group's settings: CLI > env > TOML > default,
                /// per field. `toml_section` is this group's own section
                /// (e.g. `doc.get("mcp")`), or `None` if absent/not consulted.
                pub fn resolve(
                    cli: [<Raw $name>],
                    toml_section: Option<&toml_edit::Item>,
                ) -> Self {
                    Self {
                        $(
                            $field: cli.[<$category _ $field>]
                                .clone()
                                .or_else(|| $crate::settings::env_override(
                                    stringify!($category),
                                    stringify!($field),
                                ))
                                .or_else(|| {
                                    toml_section
                                        .and_then(|s| s.get(stringify!($field)))
                                        .and_then(<$ty as $crate::settings::FromTomlItem>::from_toml_item)
                                })
                                .unwrap_or($default),
                        )+
                    }
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use std::sync::Mutex;

    /// Serializes tests that mutate process-global env vars -- `cargo test`
    /// runs unit tests in threads within one process, so two tests setting
    /// `INFIGRAPH_TOY_*` concurrently would race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    crate::settings! {
        category: toy,
        struct ToySettings {
            grace_secs: u64 = 300,
        }
    }

    #[test]
    fn resolves_hardcoded_default_when_nothing_else_set() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_GRACE_SECS");
        let cli = RawToySettings::parse_from(["test"]);
        assert_eq!(ToySettings::resolve(cli, None).grace_secs, 300);
    }

    #[test]
    fn env_overrides_hardcoded_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_GRACE_SECS", "42");
        let cli = RawToySettings::parse_from(["test"]);
        assert_eq!(ToySettings::resolve(cli, None).grace_secs, 42);
        std::env::remove_var("INFIGRAPH_TOY_GRACE_SECS");
    }

    #[test]
    fn toml_overrides_default_but_env_still_wins() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_GRACE_SECS");
        let doc: toml_edit::DocumentMut = "grace_secs = 99".parse().unwrap();
        let toml_item = doc.as_item();

        let cli = RawToySettings::parse_from(["test"]);
        assert_eq!(
            ToySettings::resolve(cli.clone(), Some(toml_item)).grace_secs,
            99
        );

        std::env::set_var("INFIGRAPH_TOY_GRACE_SECS", "42");
        assert_eq!(
            ToySettings::resolve(cli, Some(toml_item)).grace_secs,
            42
        );
        std::env::remove_var("INFIGRAPH_TOY_GRACE_SECS");
    }

    #[test]
    fn cli_overrides_everything() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_GRACE_SECS", "42");
        let cli = RawToySettings::parse_from(["test", "--toy-grace-secs", "7"]);
        assert_eq!(ToySettings::resolve(cli, None).grace_secs, 7);
        std::env::remove_var("INFIGRAPH_TOY_GRACE_SECS");
    }

    #[test]
    fn cli_flag_is_category_qualified_not_bare() {
        let bare = RawToySettings::try_parse_from(["test", "--grace-secs", "1"]);
        assert!(bare.is_err(), "bare --grace-secs must not be accepted");
        let qualified = RawToySettings::parse_from(["test", "--toy-grace-secs", "1"]);
        assert_eq!(qualified.toy_grace_secs, Some(1));
    }
}
```

- [ ] **Step 3: Wire the module into the crate**

Edit `crates/infigraph-core/src/lib.rs`. Near the top, alongside the other `pub mod` declarations, add:

```rust
pub mod settings;

/// Re-exported so a downstream crate's `infigraph_core::settings!` expansion
/// can reach `paste::paste!` via `$crate::paste::paste!` without needing its
/// own direct `paste` dependency.
pub use paste;
```

- [ ] **Step 4: Run the new tests, expect them to fail to compile first**

Run: `cargo test -p infigraph-core settings:: 2>&1 | tail -40`

Expected: compiles and the 5 tests in `crates/infigraph-core/src/settings.rs` run. If there's a compile error, fix it now — this is genuinely new macro machinery, so treat the first compile as the real test of Step 2, not a formality. Common things to check if it fails: `clap::Parser` needs `use clap::Parser;` in scope wherever `.parse_from()`/`.try_parse_from()` is called (already present in the test module above); `toml_edit::DocumentMut::as_item()` returns `&toml_edit::Item` representing the whole document as a table — confirm `doc.as_item()` is the right accessor by checking `toml_edit`'s docs for the version pinned in this workspace if it doesn't compile as written.

- [ ] **Step 5: Confirm all 5 tests pass**

Run: `cargo test -p infigraph-core settings::`
Expected: `test result: ok. 5 passed`

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy -p infigraph-core --all-targets -- -D warnings
git add crates/infigraph-core/src/settings.rs crates/infigraph-core/src/lib.rs crates/infigraph-core/Cargo.toml Cargo.lock
git commit -m "feat(core): add settings! macro (CLI + env + TOML per settings group)"
```

---

### Task 2: Migrate `idle.rs` onto the macro

**Files:**
- Modify: `crates/infigraph-mcp/src/idle.rs`
- Modify: `crates/infigraph-mcp/Cargo.toml` (add `clap` as a regular dependency)
- Test (must pass unmodified): `crates/infigraph-mcp/tests/idle_decision.rs`

**Interfaces:**
- Consumes: `infigraph_core::settings!` macro from Task 1.
- Produces: `idle_grace_period() -> Duration` and `idle_poll_interval() -> Duration` — same names/signatures as today.

- [ ] **Step 1: Add the clap dependency**

Edit `crates/infigraph-mcp/Cargo.toml`. In `[dependencies]`, add:

```toml
clap = { version = "4", features = ["derive"] }
```

(`paste` is not needed here directly — it reaches this crate via `infigraph_core::paste`, re-exported in Task 1.)

- [ ] **Step 2: Confirm the existing tests pass before touching idle.rs**

Run: `cargo test -p infigraph-mcp --test idle_decision`
Expected: `test result: ok. 3 passed` (this is the baseline — these must still pass, unmodified, after this task's changes)

- [ ] **Step 3: Rewrite idle.rs on top of the macro**

Read the current file first (`crates/infigraph-mcp/src/idle.rs`) to get its exact current doc-comments on `idle_grace_period`/`idle_poll_interval`/`should_exit_idle` and the `DEFAULT_GRACE_SECS`/`DEFAULT_POLL_SECS` constants — preserve those doc-comments verbatim on the new wrapper functions, since they carry real behavioral context (e.g. what "grace period" means) that this migration must not lose. Then replace the two settings-reading functions with:

```rust
use clap::Parser;

infigraph_core::settings! {
    category: mcp,
    struct IdleSettings {
        idle_grace_secs: u64 = DEFAULT_GRACE_SECS,
        idle_poll_secs: u64 = DEFAULT_POLL_SECS,
    }
}

// [preserve idle_grace_period's existing doc-comment here]
pub fn idle_grace_period() -> Duration {
    let cli = RawIdleSettings::parse_from(std::iter::empty::<String>());
    Duration::from_secs(IdleSettings::resolve(cli, None).idle_grace_secs)
}

// [preserve idle_poll_interval's existing doc-comment here]
pub fn idle_poll_interval() -> Duration {
    let cli = RawIdleSettings::parse_from(std::iter::empty::<String>());
    Duration::from_secs(IdleSettings::resolve(cli, None).idle_poll_secs)
}
```

`RawIdleSettings::parse_from(std::iter::empty::<String>())` deliberately does not parse this process's real `std::env::args()` — `infigraph-mcp`'s actual argv already contains `--mcp`/`--worker`, which `clap::Parser::parse()` would reject as unrecognized since `RawIdleSettings` doesn't declare them. Wiring real CLI-arg support for these settings into `infigraph-mcp`'s actual startup argv handling is a separate, later task (this spike proves the macro's shape and keeps production behavior — env var + hardcoded default only — unchanged). Passing an empty iterator gives an all-`None` `RawIdleSettings`, so `resolve()` falls straight through to env, matching today's exact behavior.

`should_exit_idle` is untouched — it doesn't read any setting itself, just compares two `Duration`s its caller already resolved.

- [ ] **Step 4: Run the existing tests, expect them to still pass unmodified**

Run: `cargo test -p infigraph-mcp --test idle_decision`
Expected: `test result: ok. 3 passed` — identical outcome to Step 2, proving zero behavior change.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy -p infigraph-mcp --all-targets -- -D warnings
git add crates/infigraph-mcp/src/idle.rs crates/infigraph-mcp/Cargo.toml Cargo.lock
git commit -m "refactor(mcp): migrate idle.rs settings onto the settings! macro"
```

---

### Task 3: Migrate `mcp_lock.rs`'s 4 numeric/duration settings onto the macro

**Files:**
- Modify: `crates/infigraph-mcp/src/mcp_lock.rs`
- Test (must pass unmodified): `crates/infigraph-mcp/tests/mcp_lock.rs`

**Interfaces:**
- Consumes: `infigraph_core::settings!` macro from Task 1; `clap::Parser` (already a dependency of `infigraph-mcp` after Task 2).
- Produces: `heartbeat_interval() -> Duration`, `wedged_threshold_secs() -> u64`, `takeover_poll_interval() -> Duration`, `takeover_wait_timeout() -> Duration` — same names/signatures as today. `lock_path()` is untouched (out of scope per Global Constraints).

- [ ] **Step 1: Confirm the existing tests pass before touching mcp_lock.rs**

Run: `cargo test -p infigraph-mcp --test mcp_lock`
Expected: all tests pass (this is the baseline)

- [ ] **Step 2: Rewrite the 4 settings functions on top of the macro**

Read the current file first (`crates/infigraph-mcp/src/mcp_lock.rs`) to preserve `heartbeat_interval`'s, `wedged_threshold_secs`'s, `takeover_poll_interval`'s, and `takeover_wait_timeout`'s exact existing doc-comments verbatim — several of these (e.g. `takeover_wait_timeout`, which `effective_takeover_wait_timeout` layers bounding logic on top of) carry real invariant context. Leave `lock_path` exactly as it is today. Replace the 4 functions with:

```rust
use clap::Parser;

infigraph_core::settings! {
    category: mcp,
    struct LockSettings {
        lock_heartbeat_secs: u64 = 15,
        lock_wedged_secs: u64 = 60,
        lock_takeover_poll_secs: u64 = 1,
        lock_takeover_timeout_secs: u64 = 10,
    }
}

fn resolved_lock_settings() -> LockSettings {
    let cli = RawLockSettings::parse_from(std::iter::empty::<String>());
    LockSettings::resolve(cli, None)
}

// [preserve heartbeat_interval's existing doc-comment here]
pub fn heartbeat_interval() -> Duration {
    Duration::from_secs(resolved_lock_settings().lock_heartbeat_secs)
}

// [preserve wedged_threshold_secs's existing doc-comment here]
pub fn wedged_threshold_secs() -> u64 {
    resolved_lock_settings().lock_wedged_secs
}

// [preserve takeover_poll_interval's existing doc-comment here]
pub fn takeover_poll_interval() -> Duration {
    Duration::from_secs(resolved_lock_settings().lock_takeover_poll_secs)
}

// [preserve takeover_wait_timeout's existing doc-comment here]
pub fn takeover_wait_timeout() -> Duration {
    Duration::from_secs(resolved_lock_settings().lock_takeover_timeout_secs)
}
```

Field names are `lock_heartbeat_secs`/`lock_wedged_secs`/`lock_takeover_poll_secs`/`lock_takeover_timeout_secs` (not the bare `heartbeat_secs`/`wedged_secs`/...) specifically so the category-qualified env var names the macro generates land on the exact existing names: category `mcp` + field `lock_heartbeat_secs` → `INFIGRAPH_MCP_LOCK_HEARTBEAT_SECS`, matching today's env var exactly (same reasoning applies to the other 3).

`resolved_lock_settings()` is a small private helper, not part of this task's public interface — it exists purely to avoid repeating the `RawLockSettings::parse_from(...)` + `LockSettings::resolve(...)` pair 4 times.

- [ ] **Step 3: Run the existing tests, expect them to still pass unmodified**

Run: `cargo test -p infigraph-mcp --test mcp_lock`
Expected: identical pass/fail outcome to Step 1, proving zero behavior change.

- [ ] **Step 4: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy -p infigraph-mcp --all-targets -- -D warnings
git add crates/infigraph-mcp/src/mcp_lock.rs
git commit -m "refactor(mcp): migrate mcp_lock.rs numeric settings onto the settings! macro"
```

---

### Task 4: Full verification and spike wrap-up

**Files:** none new — this task only runs checks and updates the spec.

- [ ] **Step 1: Run the full targeted test suite for both touched crates**

Run: `cargo test -p infigraph-core -p infigraph-mcp`
Expected: all tests pass, with no `--all` (per this Mac's disk-constrained build workflow — scoping to just the two touched crates is both faster and safer here).

- [ ] **Step 2: Confirm workspace-wide fmt and clippy are clean**

Run: `cargo fmt --all -- --check`
Expected: no output (clean)

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings/errors. If pre-existing unrelated drift elsewhere in the workspace fails this, note it separately — don't let it block this task, but don't silently `--fix` unrelated files either.

- [ ] **Step 3: Update the spec's migration-approach checkbox**

Edit `docs/superpowers/specs/2026-08-31-settings-macro-design.md`. In the "Migration approach" section, mark step 1 (the `mcp` group spike) as done by appending `— done` to that line, so a future reader of the spec doesn't have to cross-reference this plan to know the spike landed.

- [ ] **Step 4: Final commit**

```bash
git add docs/superpowers/specs/2026-08-31-settings-macro-design.md
git commit -m "docs(specs): mark the mcp idle/lock settings macro spike as done"
```
