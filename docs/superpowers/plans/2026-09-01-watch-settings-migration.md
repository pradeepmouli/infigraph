# Watch Settings Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate the `watch` settings group onto `infigraph_core::settings!`, renaming its 5 fork-only env vars to a consistent `INFIGRAPH_WATCH_*` prefix and unifying two inconsistent boolean-truthy conventions onto one.

**Architecture:** A new `Toggle` wrapper type in `settings.rs` gives the macro a reusable, permissively-parsed boolean field type. The `watch { ... }` group is declared once in `crates/infigraph-core/src/watch/mod.rs` (its natural home); each of the 5 existing accessor functions (spread across `infigraph-docs`, `infigraph-core`, and `infigraph-mcp`) is rewired to resolve through it, with its env var renamed everywhere it appears (production code, tests, doc comments).

**Tech Stack:** Rust, `macro_rules!` (`infigraph_core::settings!`), `clap`, `toml_edit`.

## Global Constraints

- All 5 renamed env vars are confirmed fork-only (zero occurrences in `upstream/main`, no README/user-doc exposure) — this is a real rename, not an aliased/back-compat shim. Every occurrence (production code, tests, doc comments) must be updated to the new name.
- `INFIGRAPH_INDEX_VIA_DAEMON` and `INFIGRAPH_AUTO_START_WATCH` currently use two different truthy conventions (strict `== "1"` vs. permissive `!= "0" && != "false"`) — both unify onto the permissive convention via the new `Toggle` type. This is a deliberate, approved small behavior change for the former.
- Out of scope, do not touch: `INFIGRAPH_WATCH_DAEMON` (dead code — no production reads), `INFIGRAPH_WATCH_ENABLED`/`INFIGRAPH_WATCH_DOCS_ENABLED` (runtime-parameterized section + per-root TOML, doesn't fit the macro), `INFIGRAPH_NO_WATCH` (part of a 6-var OR-check including non-`INFIGRAPH_*` vars), `INFIGRAPH_TEST_DAEMON_PANIC` (test-only escape hatch).
- `auto_start_watch_on_boot_enabled()` has a 3-layer precedence (env > `config.toml` `[watch].auto_start_on_boot` > hardcoded default) that the macro's `resolve()` cannot express (its TOML layer is a different, typed `serde` config file, not the macro's generic `toml_section` parameter). Do not route this field's config-file fallback through `Watch::resolve()` — read the CLI/env layer directly and fall through to `load_config_file()` manually.
- `infigraph-core` already depends on `clap` as a regular dependency (promoted during the `backend` migration). No `Cargo.toml` changes needed anywhere in this plan.
- Preserve exact existing numeric defaults: 1000ms (doc daemon poll), 600s (reap scan), 200 (storm threshold).
- Tests that mutate these env vars must keep the existing `ENV_LOCK`-style serialization pattern already present in the relevant test files (check each file for a pre-existing lock before assuming there isn't one).
- Verification per crate: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p <crate> -- --test-threads=1`, run one crate at a time (not concurrently — this machine has produced false failures from cross-process resource contention when multiple `cargo test` invocations overlap).

---

### Task 1: `Toggle` settings-field type

**Files:**
- Modify: `crates/infigraph-core/src/settings.rs`

**Interfaces:**
- Produces: `pub struct Toggle(pub bool)` implementing `FromStr` (permissive parsing) and `FromTomlItem`, for use as a `settings!` field type by Task 2's `watch` group.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/infigraph-core/src/settings.rs`, after the existing `toy_str`/`string_field_resolves_from_toml` test:

```rust
    crate::settings! {
        toy_toggle {
            flag: Toggle = Toggle(true),
        }
    }

    #[test]
    fn toggle_field_uses_permissive_truthy_parsing() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_TOGGLE_FLAG", "1");
        let cli = RawToyToggle::parse_from(["test"]);
        assert!(
            ToyToggle::resolve(cli, None).flag.0,
            "\"1\" must be treated as true"
        );

        std::env::set_var("INFIGRAPH_TOY_TOGGLE_FLAG", "0");
        let cli = RawToyToggle::parse_from(["test"]);
        assert!(
            !ToyToggle::resolve(cli, None).flag.0,
            "\"0\" must be treated as false"
        );

        std::env::set_var("INFIGRAPH_TOY_TOGGLE_FLAG", "false");
        let cli = RawToyToggle::parse_from(["test"]);
        assert!(
            !ToyToggle::resolve(cli, None).flag.0,
            "\"false\" (any case) must be treated as false"
        );

        std::env::remove_var("INFIGRAPH_TOY_TOGGLE_FLAG");
        let cli = RawToyToggle::parse_from(["test"]);
        assert!(
            ToyToggle::resolve(cli, None).flag.0,
            "unset must fall through to the hardcoded default (true)"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-core --lib settings::tests::toggle_field_uses_permissive_truthy_parsing`
Expected: FAIL to compile — `cannot find type Toggle in this scope`

- [ ] **Step 3: Write minimal implementation**

In `crates/infigraph-core/src/settings.rs`, immediately after the existing `impl FromTomlItem for String` block:

```rust
/// A settings-group boolean field with permissive truthy parsing: anything
/// except a literal "0" or case-insensitive "false" is true. Stricter
/// stdlib `bool::from_str` (only "true"/"false") would silently break the
/// "1"-means-on convention several existing `INFIGRAPH_*` toggles use.
///
/// Derives `serde::Deserialize` because the macro's generated `RawXxx`
/// struct derives it too (for every field's `Option<$ty>`), even though
/// `resolve()` doesn't actually exercise that path today -- the derive
/// bound still has to be satisfied for `RawXxx` to compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub struct Toggle(pub bool);

impl std::str::FromStr for Toggle {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Toggle(s != "0" && s.to_lowercase() != "false"))
    }
}

impl FromTomlItem for Toggle {
    fn from_toml_item(item: &toml_edit::Item) -> Option<Self> {
        item.as_bool().map(Toggle)
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p infigraph-core --lib settings::tests`
Expected: PASS (all settings.rs unit tests, including the new `toggle_field_uses_permissive_truthy_parsing`)

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/settings.rs
git commit -m "feat(core): add Toggle settings field type with permissive truthy parsing"
```

---

### Task 2: `watch` settings group + `reap_scan_interval`/`storm_threshold` migration

**Files:**
- Modify: `crates/infigraph-core/src/watch/mod.rs` (add the `settings!` declaration)
- Modify: `crates/infigraph-core/src/instances.rs`
- Modify: `crates/infigraph-core/src/watch/producer.rs`

**Interfaces:**
- Consumes: `Toggle` (Task 1)
- Produces: `RawWatch`/`Watch` structs and `Watch::resolve(cli: RawWatch, toml_section: Option<&toml_edit::Item>) -> Watch` at `infigraph_core::watch::{RawWatch, Watch}` — consumed by Tasks 3, 4, 5.

- [ ] **Step 1: Declare the `watch` settings group**

At the top of `crates/infigraph-core/src/watch/mod.rs`, immediately after the existing `use` statements (before `const COORDINATOR_TICK`), add:

```rust
crate::settings! {
    watch {
        doc_daemon_poll_ms: u64 = 1000,
        index_via_daemon: crate::settings::Toggle = crate::settings::Toggle(false),
        auto_start: crate::settings::Toggle = crate::settings::Toggle(true),
        reap_scan_secs: u64 = 600,
        storm_threshold: u64 = 200,
    }
}
```

- [ ] **Step 2: Write the failing test for `reap_scan_interval`**

In `crates/infigraph-core/tests/`, create `watch_settings.rs`:

```rust
use std::sync::Mutex;

// INFIGRAPH_WATCH_* vars are process-wide; serialize tests that set them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn reap_scan_interval_reads_renamed_env_var() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_WATCH_REAP_SCAN_SECS", "42");
    assert_eq!(
        infigraph_core::instances::reap_scan_interval().as_secs(),
        42
    );
    std::env::remove_var("INFIGRAPH_WATCH_REAP_SCAN_SECS");
    assert_eq!(
        infigraph_core::instances::reap_scan_interval().as_secs(),
        600
    );
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test watch_settings`
Expected: FAIL — `reap_scan_interval` still reads the old env var name, so setting the new one has no effect (the `42` assertion fails, or it fails to compile if `instances` isn't `pub`; check `crates/infigraph-core/src/lib.rs` for `pub mod instances;` first — it already is, per existing `mcp__infigraph__search_code` results referencing `infigraph_core::instances`).

- [ ] **Step 4: Migrate `reap_scan_interval`**

In `crates/infigraph-core/src/instances.rs`, replace:

```rust
/// How often the periodic orphan scan runs. Overridable via
/// `INFIGRAPH_REAP_SCAN_SECS` (seconds).
pub fn reap_scan_interval() -> Duration {
    std::env::var("INFIGRAPH_REAP_SCAN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(600))
}
```

with:

```rust
/// How often the periodic orphan scan runs. Overridable via
/// `INFIGRAPH_WATCH_REAP_SCAN_SECS` (seconds).
pub fn reap_scan_interval() -> Duration {
    let cli = crate::watch::RawWatch::parse_from(std::iter::empty::<String>());
    Duration::from_secs(crate::watch::Watch::resolve(cli, None).reap_scan_secs)
}
```

Add `use clap::Parser;` to `crates/infigraph-core/src/instances.rs`'s existing `use` block if it isn't already imported (check the top of the file first).

- [ ] **Step 5: Migrate `storm_threshold`**

In `crates/infigraph-core/src/watch/producer.rs`, replace:

```rust
    // R7.4 (#84): above this many files in one debounce window, the batch
    // coalesces into a single whole-project pass instead of per-file
    // updates. Overridable via INFIGRAPH_STORM_THRESHOLD for tests/tuning.
    let storm_threshold: usize = std::env::var("INFIGRAPH_STORM_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
```

with:

```rust
    // R7.4 (#84): above this many files in one debounce window, the batch
    // coalesces into a single whole-project pass instead of per-file
    // updates. Overridable via INFIGRAPH_WATCH_STORM_THRESHOLD for tests/tuning.
    let cli = crate::watch::RawWatch::parse_from(std::iter::empty::<String>());
    let storm_threshold: usize =
        crate::watch::Watch::resolve(cli, None).storm_threshold as usize;
```

Add `use clap::Parser;` to `crates/infigraph-core/src/watch/producer.rs`'s existing `use` block if not already imported.

- [ ] **Step 6: Run test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test watch_settings -- --test-threads=1`
Expected: PASS

- [ ] **Step 7: Confirm no leftover references to the old names in these two files**

Run: `mcp__infigraph__search_code` with `pattern: "INFIGRAPH_REAP_SCAN_SECS|INFIGRAPH_STORM_THRESHOLD"`, `file_pattern: "*.rs"`
Expected: zero matches anywhere in the workspace (both vars were single-file-scoped, confirmed during plan research — no test files reference them).

- [ ] **Step 8: Commit**

```bash
git add crates/infigraph-core/src/watch/mod.rs crates/infigraph-core/src/instances.rs crates/infigraph-core/src/watch/producer.rs crates/infigraph-core/tests/watch_settings.rs
git commit -m "feat(core): add watch settings group, migrate reap_scan_interval/storm_threshold"
```

---

### Task 3: Migrate `index_via_daemon_mode_enabled`

**Files:**
- Modify: `crates/infigraph-core/src/lib.rs`
- Modify: `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs` (doc comment only)
- Modify: `crates/infigraph-core/tests/daemon_kuzu_e2e.rs`
- Test: `crates/infigraph-core/tests/watch_settings.rs` (extend from Task 2)

**Interfaces:**
- Consumes: `infigraph_core::watch::{RawWatch, Watch}` (Task 2)

- [ ] **Step 1: Extend the failing test**

Append to `crates/infigraph-core/tests/watch_settings.rs`:

```rust

#[test]
fn index_via_daemon_mode_uses_permissive_truthy_and_renamed_var() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_WATCH_INDEX_VIA_DAEMON");
    assert!(!infigraph_core::index_via_daemon_mode_enabled());

    std::env::set_var("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "1");
    assert!(infigraph_core::index_via_daemon_mode_enabled());

    // Permissive convention (approved behavior change from the old
    // strict-"1"-only check): "true" now also means on.
    std::env::set_var("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "true");
    assert!(infigraph_core::index_via_daemon_mode_enabled());

    std::env::set_var("INFIGRAPH_WATCH_INDEX_VIA_DAEMON", "0");
    assert!(!infigraph_core::index_via_daemon_mode_enabled());

    std::env::remove_var("INFIGRAPH_WATCH_INDEX_VIA_DAEMON");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test watch_settings -- --test-threads=1`
Expected: FAIL — `index_via_daemon_mode_enabled()` still reads the old `INFIGRAPH_INDEX_VIA_DAEMON` name and the strict `"1"`-only convention.

- [ ] **Step 3: Migrate the accessor**

In `crates/infigraph-core/src/lib.rs`, replace:

```rust
/// Opt-in toggle for handing a whole `index()`/`index_files()` job to the
/// daemon as a single `WriteRequest::Index`, instead of the default (parse
/// locally, let the individual graph writes route themselves through the
/// daemon). Off by default; only consulted under `INFIGRAPH_BACKEND=daemon`.
///
/// Same `"1"`-means-on convention as
/// `crate::daemon::lifecycle::watch_daemon_mode_enabled`.
pub fn index_via_daemon_mode_enabled() -> bool {
    std::env::var("INFIGRAPH_INDEX_VIA_DAEMON")
        .map(|v| v == "1")
        .unwrap_or(false)
}
```

with:

```rust
/// Opt-in toggle for handing a whole `index()`/`index_files()` job to the
/// daemon as a single `WriteRequest::Index`, instead of the default (parse
/// locally, let the individual graph writes route themselves through the
/// daemon). Off by default; only consulted under `INFIGRAPH_BACKEND=daemon`.
/// Overridable via `INFIGRAPH_WATCH_INDEX_VIA_DAEMON`.
pub fn index_via_daemon_mode_enabled() -> bool {
    let cli = watch::RawWatch::parse_from(std::iter::empty::<String>());
    watch::Watch::resolve(cli, None).index_via_daemon.0
}
```

- [ ] **Step 4: Rename every remaining reference to the old env var name**

In `crates/infigraph-core/tests/daemon_kuzu_e2e.rs`, replace every occurrence of the literal string `"INFIGRAPH_INDEX_VIA_DAEMON"` with `"INFIGRAPH_WATCH_INDEX_VIA_DAEMON"` (7 occurrences: lines ~255, 316, 319, 439, 481, 614, 628, 697 — verify exact count with `mcp__infigraph__search_code` first, since line numbers may have shifted since this plan was written). This includes both literal string arguments to `.env(...)`/`run_cli_index(..., &[(...)])` calls and prose in doc comments.

In `crates/infigraph-core/src/graph/daemon_kuzu_backend.rs`, update the doc comment at (approximately) line 101 referencing `` `INFIGRAPH_INDEX_VIA_DAEMON` path `` to `` `INFIGRAPH_WATCH_INDEX_VIA_DAEMON` path ``.

In `crates/infigraph-core/src/lib.rs`, update the doc comment at (approximately) line 623 (`/// Opt-in via \`INFIGRAPH_INDEX_VIA_DAEMON=1\` (see`) to reference `INFIGRAPH_WATCH_INDEX_VIA_DAEMON=1`.

- [ ] **Step 5: Run test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test watch_settings -- --test-threads=1`
Expected: PASS

- [ ] **Step 6: Confirm no leftover references to the old name**

Run: `mcp__infigraph__search_code` with `pattern: "INFIGRAPH_INDEX_VIA_DAEMON"`, `file_pattern: "*.rs"`
Expected: zero matches (the new name, `INFIGRAPH_WATCH_INDEX_VIA_DAEMON`, is a superset string so a plain substring search for the old name alone must come back empty).

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-core/src/lib.rs crates/infigraph-core/src/graph/daemon_kuzu_backend.rs crates/infigraph-core/tests/daemon_kuzu_e2e.rs crates/infigraph-core/tests/watch_settings.rs
git commit -m "feat(core): migrate index_via_daemon_mode_enabled onto watch settings group"
```

---

### Task 4: Migrate `attach_poll_interval` (infigraph-docs)

**Files:**
- Modify: `crates/infigraph-docs/src/watch.rs`
- Modify: `crates/infigraph-cli/tests/watch_daemon_docs.rs`

**Interfaces:**
- Consumes: `infigraph_core::watch::{RawWatch, Watch}` (Task 2)

- [ ] **Step 1: Write the failing test**

In `crates/infigraph-docs/src/watch.rs`'s existing `#[cfg(test)] mod tests` block, the helpers `set_fast_poll()`/`clear_fast_poll()` currently set/remove `INFIGRAPH_DOC_DAEMON_POLL_MS`. Add a new test right after them:

```rust
    #[test]
    fn attach_poll_interval_reads_renamed_env_var() {
        std::env::set_var("INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS", "77");
        assert_eq!(attach_poll_interval().as_millis(), 77);
        std::env::remove_var("INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS");
        assert_eq!(attach_poll_interval().as_millis(), 1000);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-docs --lib watch::tests::attach_poll_interval_reads_renamed_env_var -- --test-threads=1`
Expected: FAIL — `attach_poll_interval()` still reads the old env var name.

- [ ] **Step 3: Migrate the accessor and its test helpers**

In `crates/infigraph-docs/src/watch.rs`, replace:

```rust
/// How often the daemon loop polls for `.infigraph/docs.kuzu`'s existence
/// and the per-handler stop sentinel while deciding whether to attach or
/// detach a `watch_docs` session. Overridable via
/// `INFIGRAPH_DOC_DAEMON_POLL_MS` so tests don't wait through a real 1s tick.
fn attach_poll_interval() -> Duration {
    std::env::var("INFIGRAPH_DOC_DAEMON_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(1000))
}
```

with:

```rust
/// How often the daemon loop polls for `.infigraph/docs.kuzu`'s existence
/// and the per-handler stop sentinel while deciding whether to attach or
/// detach a `watch_docs` session. Overridable via
/// `INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS` so tests don't wait through a real 1s tick.
fn attach_poll_interval() -> Duration {
    let cli = infigraph_core::watch::RawWatch::parse_from(std::iter::empty::<String>());
    Duration::from_millis(
        infigraph_core::watch::Watch::resolve(cli, None).doc_daemon_poll_ms,
    )
}
```

Add `use clap::Parser;` to the top of `crates/infigraph-docs/src/watch.rs` if not already imported — check first; `infigraph-docs` depends on `infigraph-core` but may not directly depend on `clap` itself. If `cargo build -p infigraph-docs` (Step 5 below) fails with an unresolved `clap` import, add `clap = { version = "4", features = ["derive"] }` to `crates/infigraph-docs/Cargo.toml`'s `[dependencies]` section — check first with `mcp__infigraph__search_code` for `pattern: "^clap"`, `file_pattern: "crates/infigraph-docs/Cargo.toml"` before assuming it's missing.

In the same file's test module, replace:

```rust
    fn set_fast_poll() {
        std::env::set_var("INFIGRAPH_DOC_DAEMON_POLL_MS", "20");
    }

    fn clear_fast_poll() {
        std::env::remove_var("INFIGRAPH_DOC_DAEMON_POLL_MS");
    }
```

with:

```rust
    fn set_fast_poll() {
        std::env::set_var("INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS", "20");
    }

    fn clear_fast_poll() {
        std::env::remove_var("INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS");
    }
```

- [ ] **Step 4: Rename the remaining references**

In `crates/infigraph-cli/tests/watch_daemon_docs.rs`, replace both occurrences of the literal string `"INFIGRAPH_DOC_DAEMON_POLL_MS"` with `"INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS"` (verify exact count/lines with `mcp__infigraph__search_code` first).

- [ ] **Step 5: Run test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-docs --lib watch::tests -- --test-threads=1`
Expected: PASS (all of `watch.rs`'s existing tests, including the new one)

- [ ] **Step 6: Confirm no leftover references to the old name**

Run: `mcp__infigraph__search_code` with `pattern: "INFIGRAPH_DOC_DAEMON_POLL_MS"`, `file_pattern: "*.rs"`
Expected: zero matches for the bare old name.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-docs/src/watch.rs crates/infigraph-cli/tests/watch_daemon_docs.rs
git commit -m "feat(docs): migrate attach_poll_interval onto core's watch settings group"
```

---

### Task 5: Migrate `auto_start_watch_on_boot_enabled` (infigraph-mcp)

**Files:**
- Modify: `crates/infigraph-mcp/src/session_context.rs`
- Modify: `crates/infigraph-mcp/src/recovery.rs` (doc comment only)
- Modify: `crates/infigraph-mcp/tests/startup_watch.rs`

**Interfaces:**
- Consumes: `infigraph_core::watch::{RawWatch, Watch}` (Task 2), `infigraph_core::settings::env_override` (existing, from `settings.rs`)

- [ ] **Step 1: Write the failing test**

`crates/infigraph-mcp/tests/startup_watch.rs` already has a full suite of tests exercising `INFIGRAPH_AUTO_START_WATCH`'s precedence (env > config.toml > default). Rather than duplicate that coverage, this task's correctness is proven by Step 4's rename making the existing suite pass again under the new name — the existing tests already assert exactly the CLI/env/config-file precedence this migration must preserve exactly. Confirm this understanding by reading `crates/infigraph-mcp/tests/startup_watch.rs` in full before proceeding (particularly the test around line 290 that exercises `"0"`, `"false"`, and `"1"` values together — this is the permissive-convention test that must keep passing unmodified, since `INFIGRAPH_AUTO_START_WATCH`/`INFIGRAPH_WATCH_AUTO_START` was ALREADY on the permissive convention before this migration, unlike `INDEX_VIA_DAEMON`).

- [ ] **Step 2: Run the existing suite to confirm current green baseline**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-mcp --test startup_watch -- --test-threads=1`
Expected: PASS (this is the pre-migration baseline — confirms nothing is broken yet before you start editing)

- [ ] **Step 3: Migrate the accessor**

In `crates/infigraph-mcp/src/session_context.rs`, replace:

```rust
/// Whether the MCP server should proactively start watching every
/// already-registered project on boot (daemon mode only), rather than only
/// ever starting a watcher reactively after some write happens to touch
/// that project. Priority: env var, then config.toml
/// `[watch].auto_start_on_boot`, then the hardcoded default (on). Reads the
/// config file fresh rather than going through the session-cached
/// `SESSION` static, since this must be callable from `main.rs::run()` at
/// raw process startup, before any per-session context exists.
pub fn auto_start_watch_on_boot_enabled() -> bool {
    if let Ok(v) = std::env::var("INFIGRAPH_AUTO_START_WATCH") {
        return v != "0" && v.to_lowercase() != "false";
    }
    load_config_file().watch.auto_start_on_boot
}
```

with:

```rust
/// Whether the MCP server should proactively start watching every
/// already-registered project on boot (daemon mode only), rather than only
/// ever starting a watcher reactively after some write happens to touch
/// that project. Priority: env var (`INFIGRAPH_WATCH_AUTO_START`), then
/// config.toml `[watch].auto_start_on_boot`, then the hardcoded default
/// (on). Reads the config file fresh rather than going through the
/// session-cached `SESSION` static, since this must be callable from
/// `main.rs::run()` at raw process startup, before any per-session context
/// exists.
///
/// Deliberately does NOT go through `watch::Watch::resolve()`'s own
/// hardcoded default: that would skip the `config.toml` layer entirely
/// (`resolve()` only knows CLI > env > compile-time default, with no room
/// for an externally-loaded fallback in between). Reads the CLI/env layer
/// directly instead, falling through to `load_config_file()` by hand.
pub fn auto_start_watch_on_boot_enabled() -> bool {
    let cli = infigraph_core::watch::RawWatch::parse_from(std::iter::empty::<String>());
    if let Some(v) = cli
        .watch_auto_start
        .or_else(|| infigraph_core::settings::env_override("watch", "auto_start"))
    {
        return v.0;
    }
    load_config_file().watch.auto_start_on_boot
}
```

- [ ] **Step 4: Rename every remaining reference to the old env var name**

In `crates/infigraph-mcp/tests/startup_watch.rs`, replace every occurrence of the literal string `"INFIGRAPH_AUTO_START_WATCH"` with `"INFIGRAPH_WATCH_AUTO_START"` (18 occurrences — verify exact count with `mcp__infigraph__search_code` first), including the doc-comment mentions at the top of the file and inside individual test doc comments.

In `crates/infigraph-mcp/src/recovery.rs`, update the doc comment at (approximately) line 61 referencing `` config toggle (env override: `INFIGRAPH_AUTO_START_WATCH`) `` to reference `INFIGRAPH_WATCH_AUTO_START`.

- [ ] **Step 5: Run test to verify it passes**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-mcp --test startup_watch -- --test-threads=1`
Expected: PASS (identical assertions to Step 2's baseline, now under the new env var name)

- [ ] **Step 6: Confirm no leftover references to the old name**

Run: `mcp__infigraph__search_code` with `pattern: "INFIGRAPH_AUTO_START_WATCH"`, `file_pattern: "*.rs"`
Expected: zero matches.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-mcp/src/session_context.rs crates/infigraph-mcp/src/recovery.rs crates/infigraph-mcp/tests/startup_watch.rs
git commit -m "feat(mcp): migrate auto_start_watch_on_boot_enabled onto watch settings group"
```

---

### Task 6: Full verification and spec wrap-up

**Files:**
- Modify: `docs/superpowers/specs/2026-08-31-settings-macro-design.md`

- [ ] **Step 1: Run the full targeted test suite for every touched crate, one at a time**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core -- --test-threads=1`
Expected: PASS

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-docs -- --test-threads=1`
Expected: PASS

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-mcp -- --test-threads=1`
Expected: PASS

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-cli -- --test-threads=1`
Expected: PASS

- [ ] **Step 2: Confirm no leftover references to any of the 5 old env var names anywhere in the workspace**

Run: `mcp__infigraph__search_code` with `pattern: "INFIGRAPH_DOC_DAEMON_POLL_MS|INFIGRAPH_INDEX_VIA_DAEMON|INFIGRAPH_AUTO_START_WATCH|INFIGRAPH_REAP_SCAN_SECS|INFIGRAPH_STORM_THRESHOLD"`, `file_pattern: "*.rs"`
Expected: zero matches (note: `INFIGRAPH_WATCH_AUTO_START` contains `AUTO_START` but not the old bare `INFIGRAPH_AUTO_START_WATCH` word order, so this regex will not false-positive against the new names — double check by eye if any match surfaces).

Run the same pattern against `*.md` to confirm `docs/DESIGN-hardening.md` and `docs/CONTEXT-COMPRESSION.md` — the two non-`docs/superpowers/` design docs found to mention these vars during planning — are either updated or don't need updating (read them first; only update if they document the var for a reader, not if they're just a historical plan/spec entry that's fine left as a dated record).

- [ ] **Step 3: Format and lint the whole workspace**

Run: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 4: Update the spec doc**

In `docs/superpowers/specs/2026-08-31-settings-macro-design.md`, under `## Migration approach`, change:

```
3. Remaining groups (`watch`, `graph`, `registry`, `llm`, `session`) follow once the pattern is proven, each as its own PR.
```

to:

```
3. Remaining groups (`graph`, `registry`, `llm`, `session`) follow once the pattern is proven, each as its own PR. `watch` — done (`docs/superpowers/plans/2026-09-01-watch-settings-migration.md`); its 5 real fields were all fork-only (confirmed via upstream content search) and got renamed to a consistent `INFIGRAPH_WATCH_*` prefix rather than shimmed, and two inconsistent boolean-truthy conventions were unified via a new reusable `Toggle` field type. `INFIGRAPH_WATCH_DAEMON` turned out to be dead code (removed from this bullet); `INFIGRAPH_WATCH_ENABLED`/`INFIGRAPH_WATCH_DOCS_ENABLED`/`INFIGRAPH_NO_WATCH`/`INFIGRAPH_TEST_DAEMON_PANIC` stay unmigrated (documented reasons in that plan's Global Constraints).
```

In the same doc, under `## Inventory (47 settings, current state)`, replace the `**\`watch\`**` bullet:

```
**`watch`** (collapsed `watch`/`watch_docs`): `INFIGRAPH_NO_WATCH`, `INFIGRAPH_WATCH_DAEMON`, `INFIGRAPH_WATCH_ENABLED`, `INFIGRAPH_WATCH_DOCS_ENABLED`, `INFIGRAPH_DOC_DAEMON_POLL_MS`, `INFIGRAPH_INDEX_VIA_DAEMON`, `INFIGRAPH_AUTO_START_WATCH`, `INFIGRAPH_REAP_SCAN_SECS`, `INFIGRAPH_STORM_THRESHOLD`, `INFIGRAPH_TEST_DAEMON_PANIC` (test-only escape hatch — candidate to leave un-migrated, see Open Questions)
```

with:

```
**`watch`** — done. Migrated (renamed to fit the category): `INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS`, `INFIGRAPH_WATCH_INDEX_VIA_DAEMON`, `INFIGRAPH_WATCH_AUTO_START`, `INFIGRAPH_WATCH_REAP_SCAN_SECS`, `INFIGRAPH_WATCH_STORM_THRESHOLD`. Left unmigrated (documented reasons in `docs/superpowers/plans/2026-09-01-watch-settings-migration.md`): `INFIGRAPH_NO_WATCH`, `INFIGRAPH_WATCH_ENABLED`, `INFIGRAPH_WATCH_DOCS_ENABLED`, `INFIGRAPH_TEST_DAEMON_PANIC`. `INFIGRAPH_WATCH_DAEMON` was found to be dead code (no production reads) and removed from this inventory.
```

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/specs/2026-08-31-settings-macro-design.md
git commit -m "docs(specs): mark the watch settings migration done"
```
