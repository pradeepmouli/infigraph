# Backend Settings Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate the `backend` settings group (`INFIGRAPH_BACKEND`) onto `infigraph_core::settings!`, and finish consolidating the ~10 duplicate `is_remote_mode()`/`is_neo4j_backend()` helpers scattered across every crate onto the single canonical `infigraph_core::daemon::lifecycle::is_remote_backend()`.

**Architecture:** `crates/infigraph-core/src/lib.rs` gains a `settings! { backend { selected: String = "kuzu".to_string() } }` declaration plus a `pub fn selected_backend() -> String` accessor. Because `INFIGRAPH_BACKEND` is a bare env var (no field suffix) that predates the macro's `INFIGRAPH_{CATEGORY}_{FIELD}` convention, `selected_backend()` reads the real env var itself and pre-seeds it into the generated `RawBackend`'s CLI slot before calling `resolve()` — see Task 2 for the exact reasoning. `daemon_backend_selected()` and `is_remote_backend()` become thin wrappers over `selected_backend()`. Every other crate's duplicate boolean check is then deleted in favor of calling `is_remote_backend()` directly.

**Tech Stack:** Rust, `macro_rules!` (via `infigraph_core::settings!`), `clap` (already a dependency of every touched crate), `toml_edit`.

## Global Constraints

- The env var name `INFIGRAPH_BACKEND` must not change — every existing test that calls `std::env::set_var("INFIGRAPH_BACKEND", ...)` / `std::env::remove_var("INFIGRAPH_BACKEND")` must keep passing **unmodified**.
- The resolved value stays a plain `String` (not a new enum) — this migration is settings plumbing only, not a type-safety refactor. `BackendKind` (the enum of *live, open* backend connections) is a separate concept and is untouched.
- `daemon_backend_selected()` keeps its exact name and `pub fn daemon_backend_selected() -> bool` signature — it has one external caller already relying on it (`infigraph-cli/src/index.rs`, `infigraph-mcp/src/recovery.rs`).
- `is_remote_backend()` keeps its exact name, signature, and location (`crates/infigraph-core/src/daemon/lifecycle.rs`) — it already has 2 real callers (`infigraph-cli/src/info_commands.rs:376`, `infigraph-core/src/daemon/lifecycle.rs::ensure_daemon_running_required`) that must keep compiling.
- Tests that mutate `INFIGRAPH_BACKEND` (or any macro-backed env var) must keep using the existing `ENV_LOCK`-style `Mutex<()>` serialization pattern already present in `crates/infigraph-core/tests/backend_selection.rs` and `crates/infigraph-core/src/settings.rs`.
- `cfg(feature = "remote")` / `cfg(feature = "neo4j")` / `cfg(feature = "postgres")` gates on call *sites* are preserved exactly as they are today — only the duplicate helper *functions* are deleted, never the feature gates around their callers. `is_remote_backend()` itself has no feature gate and is safe to call from inside any of those gated blocks.

---

### Task 1: `impl FromTomlItem for String` in settings.rs

**Files:**
- Modify: `crates/infigraph-core/src/settings.rs`

**Interfaces:**
- Produces: `impl FromTomlItem for String` — used by Task 2's `backend` group (its `selected` field is typed `String`).

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/infigraph-core/src/settings.rs`, right after the existing `toy_a`/`toy_b` declarations (before their closing `}`):

```rust
    crate::settings! {
        toy_str {
            name: String = "default".to_string(),
        }
    }

    #[test]
    fn string_field_resolves_from_toml() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_STR_NAME");
        let doc: toml_edit::DocumentMut = r#"name = "from-toml""#.parse().unwrap();
        let toml_item = doc.as_item();
        let cli = RawToyStr::parse_from(["test"]);
        assert_eq!(
            ToyStr::resolve(cli, Some(toml_item)).name,
            "from-toml"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-core --lib settings::tests::string_field_resolves_from_toml`
Expected: FAIL to compile — `the trait bound String: FromTomlItem is not satisfied`

- [ ] **Step 3: Write minimal implementation**

In `crates/infigraph-core/src/settings.rs`, right after the existing `impl FromTomlItem for u64` block:

```rust
impl FromTomlItem for String {
    fn from_toml_item(item: &toml_edit::Item) -> Option<Self> {
        item.as_str().map(str::to_string)
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p infigraph-core --lib settings::tests`
Expected: PASS (all settings.rs unit tests, including the new one and the pre-existing `toy_group`/`toy_a`/`toy_b` ones)

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/settings.rs
git commit -m "feat(core): add FromTomlItem impl for String settings fields"
```

---

### Task 2: `backend` settings group + `selected_backend()` accessor

**Files:**
- Modify: `crates/infigraph-core/src/lib.rs`
- Test (must pass unmodified): `crates/infigraph-core/tests/backend_selection.rs`

**Interfaces:**
- Consumes: `crate::settings!` macro (Task 1's crate), `FromTomlItem for String` (Task 1)
- Produces: `pub fn selected_backend() -> String` — the canonical resolver every other task in this plan routes through (directly, or via `daemon_backend_selected()`/`is_remote_backend()`).

**Why `selected_backend()` can't just call the macro's generated `resolve()` and trust its env layer:** `env_override()` (in `settings.rs`) always builds `INFIGRAPH_{CATEGORY}_{FIELD}`. For every other settings group that's correct (`mcp_idle`+`grace_secs` → `INFIGRAPH_MCP_IDLE_GRACE_SECS`), but `INFIGRAPH_BACKEND` is a bare env var with no field suffix that predates the macro — a naive `backend { selected: String = ... }` group would have its generated `env_override("backend", "selected")` look for `INFIGRAPH_BACKEND_SELECTED`, a different (and unset) env var, silently breaking every real deployment, test, and shell profile that sets `INFIGRAPH_BACKEND`. `selected_backend()` works around this by reading the real env var itself and feeding it into the generated struct's CLI slot — CLI already outranks env in the macro's precedence chain, so this preserves `CLI > env > TOML > default` while keeping the exact existing env var name. The macro's own `env_override("backend", "selected")` lookup for `INFIGRAPH_BACKEND_SELECTED` becomes live but practically-always-shadowed dead code (harmless — it would only matter if someone set that different, undocumented var and *not* `INFIGRAPH_BACKEND`).

- [ ] **Step 1: Write the failing test**

Add a new test file `crates/infigraph-core/tests/selected_backend.rs`:

```rust
use std::sync::Mutex;

// INFIGRAPH_BACKEND is a process-wide env var; serialize tests that set it
// so they don't race each other under cargo's default parallel test runner.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn defaults_to_kuzu_when_unset() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert_eq!(infigraph_core::selected_backend(), "kuzu");
}

#[test]
fn reads_real_env_var_name_unchanged() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
    assert_eq!(infigraph_core::selected_backend(), "neo4j");
    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn daemon_backend_selected_matches_selected_backend() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "daemon");
    assert!(infigraph_core::daemon_backend_selected());
    assert_eq!(infigraph_core::selected_backend(), "daemon");
    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn is_remote_backend_matches_selected_backend() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
    assert!(infigraph_core::daemon::lifecycle::is_remote_backend());
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert!(!infigraph_core::daemon::lifecycle::is_remote_backend());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test selected_backend`
Expected: FAIL to compile — `cannot find function selected_backend in crate infigraph_core`

- [ ] **Step 3: Write minimal implementation**

In `crates/infigraph-core/src/lib.rs`, add `use clap::Parser;` to the existing top-level `use` block (alongside `anyhow::{Context, Result}`, `rayon::prelude::*`, `sha2::{Digest, Sha256}` — see lines 25-29):

```rust
use anyhow::{Context, Result};
use clap::Parser;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
```

Then, immediately **before** the existing `pub fn daemon_backend_selected() -> bool` (`crates/infigraph-core/src/lib.rs:218`), add:

```rust
crate::settings! {
    backend {
        selected: String = "kuzu".to_string(),
    }
}

/// Resolves the active backend selector: CLI > env > TOML > default
/// (`"kuzu"`). The single source of truth for `INFIGRAPH_BACKEND` --
/// `daemon_backend_selected()` and `daemon::lifecycle::is_remote_backend()`
/// are both thin wrappers over this.
///
/// `INFIGRAPH_BACKEND` predates the `settings!` macro and has no field
/// suffix, so it can't go through the macro's generic
/// `INFIGRAPH_{CATEGORY}_{FIELD}` env lookup (that would read
/// `INFIGRAPH_BACKEND_SELECTED` instead). Read it directly and seed it into
/// the CLI slot instead, which still outranks env/TOML/default in the
/// macro's own precedence chain.
pub fn selected_backend() -> String {
    let mut cli = RawBackend::parse_from(std::iter::empty::<String>());
    cli.backend_selected = cli
        .backend_selected
        .or_else(|| std::env::var("INFIGRAPH_BACKEND").ok());
    Backend::resolve(cli, None).selected
}
```

Then replace the existing `daemon_backend_selected()` body (`crates/infigraph-core/src/lib.rs:218-222`):

```rust
pub fn daemon_backend_selected() -> bool {
    std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "daemon")
        .unwrap_or(false)
}
```

with:

```rust
pub fn daemon_backend_selected() -> bool {
    selected_backend() == "daemon"
}
```

Then, in `crates/infigraph-core/src/daemon/lifecycle.rs`, replace `is_remote_backend()`'s body (lines 39-43):

```rust
pub fn is_remote_backend() -> bool {
    std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false)
}
```

with:

```rust
pub fn is_remote_backend() -> bool {
    crate::selected_backend() == "neo4j"
}
```

Finally, replace the three remaining raw reads in `crates/infigraph-core/src/lib.rs` — each is currently `let backend_env = std::env::var("INFIGRAPH_BACKEND").unwrap_or_else(|_| "kuzu".into());` at lines **307** (`Infigraph::init`), **494** (`Infigraph::init_read_only`), and **528** (`Infigraph::init_read_only_or_degrade`) — with:

```rust
        let backend_env = selected_backend();
```

(same three locations, same variable name and downstream `match backend_env.as_str() { ... }` logic — only the right-hand side changes.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p infigraph-core --test selected_backend --test backend_selection --lib settings::tests -- --test-threads=1`
Expected: PASS — the new `selected_backend` tests, plus the pre-existing `backend_selection.rs` tests (`init_selects_daemon_kuzu_backend_when_env_var_set`, `init_selects_kuzu_backend_by_default`) unmodified.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/lib.rs crates/infigraph-core/src/daemon/lifecycle.rs crates/infigraph-core/tests/selected_backend.rs
git commit -m "feat(core): migrate backend selection onto the settings! macro"
```

---

### Task 3: Consolidate infigraph-mcp's duplicate `is_remote_mode()`

**Files:**
- Modify: `crates/infigraph-mcp/src/tools/docs.rs`
- Modify: `crates/infigraph-mcp/src/tools/groups.rs`
- Modify: `crates/infigraph-mcp/src/tools/index.rs`
- Modify: `crates/infigraph-mcp/src/tools/helpers.rs`
- Modify: `crates/infigraph-mcp/src/tools/watch.rs`
- Modify: `crates/infigraph-mcp/src/tools/search.rs`
- Modify: `crates/infigraph-mcp/src/health.rs`

**Interfaces:**
- Consumes: `infigraph_core::daemon::lifecycle::is_remote_backend() -> bool` (Task 2)

Each file below has an identical duplicate of:
```rust
fn is_remote_mode() -> bool {
    std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false)
}
```
(some `#[cfg(feature = "remote")]`-gated, `search.rs`'s wraps it in an inner `#[cfg]`/`#[cfg(not)]` pair). Delete the duplicate function in each file and change every call site from `is_remote_mode()` to `infigraph_core::daemon::lifecycle::is_remote_backend()`.

- [ ] **Step 1: `docs.rs`** — delete the `fn is_remote_mode()` definition (currently right before `fn auto_start_doc_watch_inner`), then in `auto_start_doc_watch_inner`:
```rust
    if is_remote_mode() {
```
→
```rust
    if infigraph_core::daemon::lifecycle::is_remote_backend() {
```

- [ ] **Step 2: `groups.rs`** — delete the `#[cfg(feature = "remote")] fn is_remote_mode()` definition (currently right after the `use` block), then at its 3 call sites (`tool_group_link_docs`, `tool_group_search`, `tool_group_search_docs`, each reading `#[cfg(feature = "remote")] if is_remote_mode() {`):
```rust
    #[cfg(feature = "remote")]
    if is_remote_mode() {
```
→
```rust
    #[cfg(feature = "remote")]
    if infigraph_core::daemon::lifecycle::is_remote_backend() {
```
Then at the separate inline site (Step 4 of the "Build combined graph" block):
```rust
    let is_remote = {
        #[cfg(feature = "remote")]
        {
            std::env::var("INFIGRAPH_BACKEND")
                .map(|v| v == "neo4j")
                .unwrap_or(false)
        }
        #[cfg(not(feature = "remote"))]
        {
            false
        }
    };
```
→
```rust
    let is_remote = {
        #[cfg(feature = "remote")]
        {
            infigraph_core::daemon::lifecycle::is_remote_backend()
        }
        #[cfg(not(feature = "remote"))]
        {
            false
        }
    };
```

- [ ] **Step 3: `index.rs`** — delete the `#[cfg(feature = "remote")] fn is_remote_mode()` definition, then in `tool_index_project`'s `#[cfg(feature = "remote")] if is_remote_mode() {`:
→ `#[cfg(feature = "remote")] if infigraph_core::daemon::lifecycle::is_remote_backend() {`

- [ ] **Step 4: `helpers.rs`** — in `apply_repo_filter` (already `#[cfg(feature = "remote")]`-gated at the function level):
```rust
    if std::env::var("INFIGRAPH_BACKEND").as_deref() != Ok("neo4j") {
        return;
    }
```
→
```rust
    if !infigraph_core::daemon::lifecycle::is_remote_backend() {
        return;
    }
```

- [ ] **Step 5: `watch.rs`** — delete the `fn is_remote_mode()` definition, then at its 2 call sites (`auto_start_watch_inner`, `tool_watch_project`), each `if is_remote_mode() {` → `if infigraph_core::daemon::lifecycle::is_remote_backend() {`

- [ ] **Step 6: `search.rs`** — delete the `fn is_remote_mode()` definition (the one with the inner `#[cfg]`/`#[cfg(not)]` pair), then at its 4 call sites (`get_or_build_search_ctx`, `tool_search`, `tool_search_symbols`, `tool_semantic_search`), each `is_remote_mode()` → `infigraph_core::daemon::lifecycle::is_remote_backend()`

- [ ] **Step 7: `health.rs`** — delete the `fn is_remote_mode()` definition, then in `gather_signals`'s call site: `is_remote_mode()` → `infigraph_core::daemon::lifecycle::is_remote_backend()`

- [ ] **Step 8: Compile-check the whole crate under both feature configurations**

Run: `cargo build -p infigraph-mcp && cargo build -p infigraph-mcp --features remote`
Expected: both succeed with no warnings about unused `is_remote_mode` or dead `#[cfg]` arms.

- [ ] **Step 9: Run the crate's test suite**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-mcp -- --test-threads=1`
Expected: PASS (no behavior change — same boolean logic, same env var).

- [ ] **Step 10: Commit**

```bash
git add crates/infigraph-mcp/src/tools/docs.rs crates/infigraph-mcp/src/tools/groups.rs crates/infigraph-mcp/src/tools/index.rs crates/infigraph-mcp/src/tools/helpers.rs crates/infigraph-mcp/src/tools/watch.rs crates/infigraph-mcp/src/tools/search.rs crates/infigraph-mcp/src/health.rs
git commit -m "refactor(mcp): consolidate duplicate is_remote_mode onto core's is_remote_backend"
```

---

### Task 4: Consolidate infigraph-cli's duplicate `is_neo4j_backend()`/inline checks

**Files:**
- Modify: `crates/infigraph-cli/src/index.rs`
- Modify: `crates/infigraph-cli/src/group_commands.rs`
- Modify: `crates/infigraph-cli/src/info_commands.rs`
- Modify: `crates/infigraph-cli/src/main.rs`

**Interfaces:**
- Consumes: `infigraph_core::daemon::lifecycle::is_remote_backend() -> bool` (Task 2)

- [ ] **Step 1: `index.rs`** — delete the `#[cfg(feature = "remote")] fn is_neo4j_backend()` definition, then at its one call site:
```rust
    #[cfg(feature = "remote")]
    let remote = is_neo4j_backend();
```
→
```rust
    #[cfg(feature = "remote")]
    let remote = infigraph_core::daemon::lifecycle::is_remote_backend();
```

- [ ] **Step 2: `group_commands.rs`** — replace the inline block:
```rust
            let is_remote = {
                #[cfg(feature = "remote")]
                {
                    std::env::var("INFIGRAPH_BACKEND")
                        .map(|v| v == "neo4j")
                        .unwrap_or(false)
                }
                #[cfg(not(feature = "remote"))]
                {
                    false
                }
            };
```
→
```rust
            let is_remote = {
                #[cfg(feature = "remote")]
                {
                    infigraph_core::daemon::lifecycle::is_remote_backend()
                }
                #[cfg(not(feature = "remote"))]
                {
                    false
                }
            };
```

- [ ] **Step 3: `info_commands.rs`** — replace:
```rust
    #[cfg(feature = "remote")]
    let is_remote = std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false);
    #[cfg(not(feature = "remote"))]
    let is_remote = false;
```
→
```rust
    #[cfg(feature = "remote")]
    let is_remote = infigraph_core::daemon::lifecycle::is_remote_backend();
    #[cfg(not(feature = "remote"))]
    let is_remote = false;
```
(Leave the existing call at `info_commands.rs:376` inside `cmd_daemon` untouched — it already calls `is_remote_backend()`.)

- [ ] **Step 4: `main.rs`** — replace:
```rust
            #[cfg(feature = "remote")]
            let doc_ns = if std::env::var("INFIGRAPH_BACKEND")
                .map(|v| v == "neo4j")
                .unwrap_or(false)
            {
```
→
```rust
            #[cfg(feature = "remote")]
            let doc_ns = if infigraph_core::daemon::lifecycle::is_remote_backend() {
```
(the rest of the `if`/`else` block — `infigraph_core::multi::Registry::load()...` / `} else { None };` — is unchanged.)

- [ ] **Step 5: Compile-check under both feature configurations**

Run: `cargo build -p infigraph-cli && cargo build -p infigraph-cli --features remote`
Expected: both succeed.

- [ ] **Step 6: Run the crate's test suite**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-cli -- --test-threads=1`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-cli/src/index.rs crates/infigraph-cli/src/group_commands.rs crates/infigraph-cli/src/info_commands.rs crates/infigraph-cli/src/main.rs
git commit -m "refactor(cli): consolidate duplicate remote-backend checks onto core's is_remote_backend"
```

---

### Task 5: Consolidate infigraph-docs's duplicate `is_remote_mode()`

**Files:**
- Modify: `crates/infigraph-docs/src/lib.rs`
- Modify: `crates/infigraph-docs/src/search.rs`

**Interfaces:**
- Consumes: `infigraph_core::daemon::lifecycle::is_remote_backend() -> bool` (Task 2) — already a dependency of `infigraph-docs` (`infigraph-core = { path = "../infigraph-core" }` in `Cargo.toml`, unconditional — not gated behind `infigraph-docs`'s own `remote` feature).

- [ ] **Step 1: `lib.rs`** — delete the `fn is_remote_mode()` definition, then at its 3 call sites (`DocIndex::open`, `DocIndex::init`, `DocIndex::index`): `is_remote_mode()` → `infigraph_core::daemon::lifecycle::is_remote_backend()`

- [ ] **Step 2: `search.rs`** — delete the `#[cfg(feature = "remote")] fn is_remote_mode()` definition, then at its call site in `hybrid_doc_search`: `is_remote_mode()` → `infigraph_core::daemon::lifecycle::is_remote_backend()`

- [ ] **Step 3: Compile-check under both feature configurations**

Run: `cargo build -p infigraph-docs && cargo build -p infigraph-docs --features remote`
Expected: both succeed.

- [ ] **Step 4: Run the crate's test suite**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-docs -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-docs/src/lib.rs crates/infigraph-docs/src/search.rs
git commit -m "refactor(docs): consolidate duplicate is_remote_mode onto core's is_remote_backend"
```

---

### Task 6: Consolidate infigraph-core's own `multi/mod.rs` duplicates

**Files:**
- Modify: `crates/infigraph-core/src/multi/mod.rs`

**Interfaces:**
- Consumes: `crate::daemon::lifecycle::is_remote_backend() -> bool` (Task 2) — same-crate call, no new dependency.

- [ ] **Step 1:** Replace the `#[cfg(feature = "neo4j")]`-gated block (currently around line 719):
```rust
    #[cfg(feature = "neo4j")]
    let neo4j_backend = if !full
        && std::env::var("INFIGRAPH_BACKEND")
            .map(|v| v == "neo4j")
            .unwrap_or(false)
    {
```
→
```rust
    #[cfg(feature = "neo4j")]
    let neo4j_backend = if !full && crate::daemon::lifecycle::is_remote_backend() {
```

- [ ] **Step 2:** Replace `use_parallel`'s definition (currently around line 788):
```rust
    let use_parallel = std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false);
```
→
```rust
    let use_parallel = crate::daemon::lifecycle::is_remote_backend();
```

- [ ] **Step 3:** Replace `remote_namespace`'s `in_remote` (currently around line 984):
```rust
    let in_remote = std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false);
```
→
```rust
    let in_remote = crate::daemon::lifecycle::is_remote_backend();
```

- [ ] **Step 4:** Delete the `#[cfg(feature = "postgres")] fn is_remote_mode()` definition (currently around line 994), then at its 2 call sites (`Registry::load`, `Registry::save`): `is_remote_mode()` → `crate::daemon::lifecycle::is_remote_backend()`

- [ ] **Step 5: Compile-check under all relevant feature combinations**

Run: `cargo build -p infigraph-core && cargo build -p infigraph-core --features neo4j,postgres`
Expected: both succeed.

- [ ] **Step 6: Run the crate's test suite**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core --test group_build_steps -- --test-threads=1`
Expected: PASS (this is the test file exercising `Registry::load`/`save` under `INFIGRAPH_BACKEND=neo4j`).

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-core/src/multi/mod.rs
git commit -m "refactor(core): consolidate multi/mod.rs's duplicate remote-backend checks"
```

---

### Task 7: Full verification and spec wrap-up

**Files:**
- Modify: `docs/superpowers/specs/2026-08-31-settings-macro-design.md` (mark `backend` migration done, same style as the existing "spike ... — done" note under "Migration approach")

- [ ] **Step 1: Run the full targeted test suite for every touched crate**

Run: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p infigraph-core -p infigraph-cli -p infigraph-mcp -p infigraph-docs -- --test-threads=1`
Expected: PASS, zero failures.

- [ ] **Step 2: Confirm no leftover `INFIGRAPH_BACKEND` raw reads outside test files and the intentional subprocess-env strip sites**

Run: `mcp__infigraph__search_code` with `pattern: "std::env::var\\(\"INFIGRAPH_BACKEND\"\\)"`, `file_pattern: "*.rs"`
Expected: zero matches outside `crates/infigraph-core/src/lib.rs`'s three `backend_env` sites (now themselves migrated in Task 2 — so really zero matches at all), test files, and the `.env_remove("INFIGRAPH_BACKEND")`/`std::env::remove_var("INFIGRAPH_BACKEND")`/`std::env::set_var("INFIGRAPH_BACKEND", "daemon")` subprocess-env-manipulation sites in `lib.rs:1650/1653` and `daemon/lifecycle.rs:426` (those are intentionally untouched per this plan's Global Constraints — they control a *child* process's environment, not this process's settings resolution).

- [ ] **Step 3: Format and lint the whole workspace**

Run: `cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings`
Expected: both clean. Per this repo's `CLAUDE.md`, the pre-commit hook enforces this workspace-wide, so pre-existing unrelated drift elsewhere can block an otherwise-clean change — investigate before assuming this PR caused it.

- [ ] **Step 4: Update the spec doc**

In `docs/superpowers/specs/2026-08-31-settings-macro-design.md`, under `## Migration approach`, change:
```
2. Migrate **`backend`** next — highest duplication payoff (15+ call sites collapse to one `Settings::backend()` accessor), and the biggest DRY win in the codebase per the user's global #1 rule.
```
to:
```
2. Migrate **`backend`** next — highest duplication payoff (15+ call sites collapse to one `Settings::backend()` accessor), and the biggest DRY win in the codebase per the user's global #1 rule. — done (`docs/superpowers/plans/2026-08-31-backend-settings-migration.md`). Also consolidated the ~10 duplicate `is_remote_mode()`/`is_neo4j_backend()` helpers found still live across infigraph-mcp/infigraph-cli/infigraph-docs/infigraph-core onto the pre-existing (but previously unused-for-this-purpose) `daemon::lifecycle::is_remote_backend()`.
```

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/specs/2026-08-31-settings-macro-design.md
git commit -m "docs(specs): mark the backend settings migration done"
```
