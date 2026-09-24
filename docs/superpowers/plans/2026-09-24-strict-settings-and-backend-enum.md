# Strict Settings + Backend Enum Implementation Plan (#74)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop settings from silently falling back to a default when a value doesn't parse. Add enum-valued settings to the `settings!` macro, and use one to make `INFIGRAPH_BACKEND` a validated enum. A typo'd backend then fails at startup instead of silently running the wrong backend (#74 / I-6).

**Architecture:** `settings!` gains four things:
- An optional `= default` per field. A field without one is required.
- A `#[legacy = "NAME"]` attribute, replacing the hand-written `legacy_env` seeding at seven sites.
- A strict `resolve()` that returns `Result<Self, SettingsError>` and collects every bad value.
- For groups whose every field declares a default, and only those: a generated `impl Default` and a `resolve_or_default()` that logs a bad value and uses that field's default.

A companion `settings_enum!` generates an enum with `FromStr` (whose error lists the valid values), `as_str`, `Display`, `FromTomlItem` and `Deserialize`. The backend becomes `BackendChoice`, which is validated strictly at CLI/MCP startup and in `Infigraph::init*`. The bool helpers stay infallible.

**Tech Stack:** Rust `macro_rules!` + `paste` (no proc-macro), clap derive, `toml_edit`, serde.

**Spec:** `docs/superpowers/specs/2026-08-31-settings-macro-design.md` (macro conventions), GitHub #74 (triage comment 2026-09-23), and the user rulings of 2026-09-24 recorded below.

## Global Constraints

- **User rulings (2026-09-24), do not re-open:**
  1. Enums come from a declarative `settings_enum!` companion macro, not hand-written impls and not a proc-macro.
  2. Strict everywhere: an unparseable value is an error for every group; `resolve()` returns `Result`.
  3. The backend is validated once at CLI/MCP startup and in `Infigraph::init*`; `daemon_backend_selected()` / `is_remote_backend()` stay infallible.
  4. A bad value in an infallible helper falls back to the macro-generated default and is logged.
  5. A default is generated only when one is declared: `impl Default` and `resolve_or_default()` exist only for groups where every field has `= default`.
- Precedence is unchanged: CLI > legacy env name > `INFIGRAPH_{CATEGORY}_{FIELD}` > TOML (nearest layer first) > default.
- `INFIGRAPH_BACKEND` keeps its exact name; `LOCAL_BACKEND`/`DAEMON_BACKEND` stay `&'static str` constants, so the ~80 test sites that pass them to `.env(...)` do not change.
- Warnings use the core convention: `eprintln!("warning: ...")`.
- Tests run with `INFIGRAPH_BACKEND=kuzu` pinned and `INFIGRAPH_WATCH_DAEMON` unset (`env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu`).
- No new dependencies.

## Review Focus

1. **A bad value in `~/.infigraph/config.toml` while a daemon is running.** The daemon must keep running on the default and warn, never panic. `resolve_or_default` is the only thing its helpers call. Pinned by Task 2's `resolve_or_default_uses_the_default_for_a_bad_value`.
2. **`infigraph doctor` with a broken backend.** Doctor must still run and report the backend problem, not refuse at startup. Pinned by Task 4's `doctor_runs_and_reports_an_invalid_backend`.
3. **An empty `INFIGRAPH_BACKEND=`,** which is common in shell profiles. It is set, so it must be an error naming the variable, not a silent `daemon`. Pinned by Task 3's `empty_backend_is_an_error_not_the_default`.
4. **A legacy name and the convention name both set.** The legacy name wins, as today. Pinned by Task 1's `legacy_name_outranks_the_convention_name`.
5. **Two bad values at once.** Both are reported in one error, not just the first. Pinned by Task 1's `every_bad_value_is_reported_not_just_the_first`.

---

### Task 1: Strict `settings!`: optional defaults, `#[legacy]`, `resolve() -> Result`

**Files:**
- Modify: `crates/infigraph-core/src/settings.rs` (whole macro, `FromTomlItem`, env helpers, tests)

**Interfaces:**
- Produces:
  - `pub struct SettingError { pub setting: String, pub problem: String }` (`Display`: `"{setting}: {problem}"`)
  - `pub struct SettingsError(pub Vec<SettingError>)` (`Display` joins with `"; "`, `impl std::error::Error`)
  - `pub fn env_name(category: &str, field: &str) -> String`, giving `INFIGRAPH_{CATEGORY}_{FIELD}`
  - `pub fn env_value<T: FromStr>(name: &str) -> Result<Option<T>, SettingError> where T::Err: Display`
  - `pub trait FromTomlItem { fn from_toml_item(item: &toml_edit::Item) -> Result<Self, String>; }`
  - Per group: `resolve(cli, scope) -> Result<Self, SettingsError>`, `resolve_layers(cli, layers) -> Result<Self, SettingsError>`, `env_layer() -> Result<Raw…, SettingsError>` (legacy + convention env only)
  - Per all-default group only: `impl Default`, `resolve_or_default(cli, scope) -> Self`, `resolve_layers_or_default(cli, layers) -> Self`
- Removes: `env_override`, `legacy_env` (their callers move in Task 2).

- [ ] **Step 1: Write the failing tests** (append to `settings.rs`'s `mod tests`; existing tests gain `.unwrap()` on `resolve_layers`/`resolve`)

```rust
    crate::settings! {
        toy_req {
            name: String,
            count: u64 = 3,
        }
    }

    crate::settings! {
        toy_legacy {
            #[legacy = "INFIGRAPH_TOY_OLD_NAME"]
            value: u64 = 1,
        }
    }

    #[test]
    fn a_bad_env_value_is_an_error_naming_the_variable_and_value() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_GROUP_GRACE_SECS", "soon");
        let err = ToyGroup::resolve_layers(RawToyGroup::parse_from(["test"]), &[]).unwrap_err();
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
        let msg = err.to_string();
        assert!(msg.contains("INFIGRAPH_TOY_GROUP_GRACE_SECS"), "{msg}");
        assert!(msg.contains("\"soon\""), "{msg}");
    }

    #[test]
    fn a_bad_toml_value_is_an_error_naming_the_key() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
        let doc: toml_edit::DocumentMut = "[toy_group]\ngrace_secs = \"soon\"".parse().unwrap();
        let err = ToyGroup::resolve_layers(RawToyGroup::parse_from(["test"]), &[doc.as_item()])
            .unwrap_err();
        assert!(err.to_string().contains("[toy_group] grace_secs"), "{err}");
    }

    #[test]
    fn every_bad_value_is_reported_not_just_the_first() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_PAIR_NEAR", "x");
        std::env::set_var("INFIGRAPH_TOY_PAIR_FAR", "y");
        let err = ToyPair::resolve_layers(RawToyPair::parse_from(["test"]), &[]).unwrap_err();
        std::env::remove_var("INFIGRAPH_TOY_PAIR_NEAR");
        std::env::remove_var("INFIGRAPH_TOY_PAIR_FAR");
        assert_eq!(err.0.len(), 2, "{err}");
    }

    #[test]
    fn a_required_field_that_is_unset_is_an_error() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_REQ_NAME");
        let err = ToyReq::resolve_layers(RawToyReq::parse_from(["test"]), &[]).unwrap_err();
        assert!(err.to_string().contains("INFIGRAPH_TOY_REQ_NAME"), "{err}");
        assert!(err.to_string().contains("required"), "{err}");

        std::env::set_var("INFIGRAPH_TOY_REQ_NAME", "x");
        let got = ToyReq::resolve_layers(RawToyReq::parse_from(["test"]), &[]).unwrap();
        std::env::remove_var("INFIGRAPH_TOY_REQ_NAME");
        assert_eq!((got.name.as_str(), got.count), ("x", 3));
    }

    #[test]
    fn legacy_name_outranks_the_convention_name() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_OLD_NAME", "7");
        std::env::set_var("INFIGRAPH_TOY_LEGACY_VALUE", "9");
        let got = ToyLegacy::resolve_layers(RawToyLegacy::parse_from(["test"]), &[]).unwrap();
        std::env::remove_var("INFIGRAPH_TOY_LEGACY_VALUE");
        assert_eq!(got.value, 7);

        std::env::set_var("INFIGRAPH_TOY_OLD_NAME", "seven");
        let err = ToyLegacy::resolve_layers(RawToyLegacy::parse_from(["test"]), &[]).unwrap_err();
        std::env::remove_var("INFIGRAPH_TOY_OLD_NAME");
        assert!(err.to_string().contains("INFIGRAPH_TOY_OLD_NAME"), "{err}");
    }

    #[test]
    fn resolve_or_default_uses_the_default_for_a_bad_value() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_PAIR_NEAR", "x");
        std::env::set_var("INFIGRAPH_TOY_PAIR_FAR", "20");
        let got = ToyPair::resolve_layers_or_default(RawToyPair::parse_from(["test"]), &[]);
        std::env::remove_var("INFIGRAPH_TOY_PAIR_NEAR");
        std::env::remove_var("INFIGRAPH_TOY_PAIR_FAR");
        // Only the bad field falls back; the good one keeps its value.
        assert_eq!(got, ToyPair { near: 1, far: 20 });
    }

    #[test]
    fn default_impl_is_built_from_the_declared_defaults() {
        assert_eq!(ToyPair::default(), ToyPair { near: 1, far: 2 });
    }

    #[test]
    fn a_non_array_path_list_in_toml_is_an_error() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_PATHS_INCLUDE");
        let doc: toml_edit::DocumentMut = "[toy_paths]\ninclude = \"vendor\"".parse().unwrap();
        assert!(ToyPaths::resolve_layers(RawToyPaths::parse_from(["test"]), &[doc.as_item()]).is_err());
    }
```

Add this doc test on the `settings!` macro's doc comment. It proves `Default` is generated only when every field has a default. The second block is the control that shows the first fails for the right reason:

```rust
/// A group with a required field gets no `Default`:
///
/// ```compile_fail
/// infigraph_core::settings! { doc_req { name: String } }
/// let _ = DocReq::default();
/// ```
///
/// ```
/// infigraph_core::settings! { doc_def { name: String = String::new() } }
/// let _ = DocDef::default();
/// ```
```

- [ ] **Step 2: Run the tests to confirm they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib settings::`
Expected: compile errors (`ToyReq` field without default, `#[legacy]`, `unwrap_err` on non-Result).

- [ ] **Step 3: Implement.** Replace `env_override`/`legacy_env`/`FromTomlItem` and the macro in `settings.rs` with:

```rust
/// One setting that could not be resolved: `setting` names where it came
/// from (`INFIGRAPH_X_Y`, or `[x] y` for config.toml), `problem` says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingError {
    pub setting: String,
    pub problem: String,
}

impl std::fmt::Display for SettingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.setting, self.problem)
    }
}

/// Every setting in a group that could not be resolved -- all of them, so
/// one run shows every typo rather than one per attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError(pub Vec<SettingError>);

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self.0.iter().map(ToString::to_string).collect();
        f.write_str(&parts.join("; "))
    }
}

impl std::error::Error for SettingsError {}

/// `INFIGRAPH_{CATEGORY}_{FIELD}`, both upper-cased.
pub fn env_name(category: &str, field: &str) -> String {
    format!(
        "INFIGRAPH_{}_{}",
        category.to_ascii_uppercase(),
        field.to_ascii_uppercase()
    )
}

/// Reads and parses env var `name`. Unset is `Ok(None)`; set but
/// unparseable -- including empty -- is an error naming the variable and
/// the value, never a silent fall-through to the next layer.
pub fn env_value<T: std::str::FromStr>(name: &str) -> Result<Option<T>, SettingError>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(SettingError {
            setting: name.to_string(),
            problem: "is not valid UTF-8".to_string(),
        }),
        Ok(raw) => raw.parse().map(Some).map_err(|e| SettingError {
            setting: name.to_string(),
            problem: format!("{raw:?} is not valid: {e}"),
        }),
    }
}

/// Reads one field out of a `toml_edit` item. `Err` carries why the value
/// is unusable; a key that is absent never reaches this.
pub trait FromTomlItem: Sized {
    fn from_toml_item(item: &toml_edit::Item) -> Result<Self, String>;
}

impl FromTomlItem for u64 {
    fn from_toml_item(item: &toml_edit::Item) -> Result<Self, String> {
        let i = item.as_integer().ok_or("expected an integer")?;
        u64::try_from(i).map_err(|_| format!("{i} must not be negative"))
    }
}

impl FromTomlItem for String {
    fn from_toml_item(item: &toml_edit::Item) -> Result<Self, String> {
        item.as_str().map(str::to_string).ok_or_else(|| "expected a string".to_string())
    }
}
```

`Toggle` keeps its permissive `FromStr` (it cannot fail). Its TOML impl becomes `item.as_bool().map(Toggle).ok_or_else(|| "expected true or false".to_string())`. `PathList`'s TOML impl returns `Err("expected an array of strings")` for a non-array, or for any non-string entry. Update its doc comment, which today says a non-array "states nothing".

The macro:

```rust
#[doc(hidden)]
#[macro_export]
macro_rules! __settings_default {
    () => {
        None
    };
    ($default:expr) => {
        Some($default)
    };
}

#[macro_export]
macro_rules! settings {
    // Every field declares a default: the group also gets `Default` and the
    // logged-fallback resolvers. Tried first; a group with any required
    // field fails this arm and takes the next one.
    (
        $category:ident {
            $( $(#[legacy = $legacy:literal])? $field:ident : $ty:ty = $default:expr ),+ $(,)?
        }
    ) => {
        $crate::settings!(@group $category {
            $( $(#[legacy = $legacy])? $field : $ty = $default ),+
        });

        $crate::paste::paste! {
            impl Default for [<$category:camel>] {
                fn default() -> Self {
                    Self { $( $field: $default, )+ }
                }
            }

            impl [<$category:camel>] {
                /// [`resolve`](Self::resolve), but a value that does not
                /// parse is logged and replaced by that field's declared
                /// default instead of failing -- for the infallible helpers
                /// that run inside long-lived processes, where a config
                /// file edited mid-run must not take the process down.
                #[allow(dead_code)]
                pub fn resolve_or_default(
                    cli: [<Raw $category:camel>],
                    scope: $crate::settings_file::ConfigScope<'_>,
                ) -> Self {
                    let docs = $crate::settings_file::layers(scope);
                    let layers: Vec<&$crate::toml_edit::Item> =
                        docs.iter().flatten().map(|doc| doc.as_item()).collect();
                    Self::resolve_layers_or_default(cli, &layers)
                }

                /// [`resolve_or_default`](Self::resolve_or_default) over
                /// explicit config documents, nearest layer first.
                pub fn resolve_layers_or_default(
                    cli: [<Raw $category:camel>],
                    layers: &[&$crate::toml_edit::Item],
                ) -> Self {
                    Self {
                        $(
                            $field: match Self::[<resolve_ $field>](&cli, layers) {
                                Ok(Some(value)) => value,
                                Ok(None) => $default,
                                Err(e) => {
                                    eprintln!("warning: {e}; using the default");
                                    $default
                                }
                            },
                        )+
                    }
                }
            }
        }
    };
    // Some field is required (no `= default`): strict resolution only.
    (
        $category:ident {
            $( $(#[legacy = $legacy:literal])? $field:ident : $ty:ty $(= $default:expr)? ),+ $(,)?
        }
    ) => {
        $crate::settings!(@group $category {
            $( $(#[legacy = $legacy])? $field : $ty $(= $default)? ),+
        });
    };
    (@group $category:ident {
        $( $(#[legacy = $legacy:literal])? $field:ident : $ty:ty $(= $default:expr)? ),+
    }) => {
        $crate::paste::paste! {
            #[derive(Debug, Clone, Default, clap::Parser, serde::Deserialize)]
            pub struct [<Raw $category:camel>] {
                $(
                    #[arg(long)]
                    pub [<$category _ $field>]: Option<$ty>,
                )+
            }

            #[derive(Debug, Clone, PartialEq)]
            pub struct [<$category:camel>] {
                $( pub $field: $ty, )+
            }

            impl [<$category:camel>] {
                /// Resolves this group: CLI > legacy env name > convention
                /// env name > TOML (nearest layer first) > declared default.
                /// Every value that does not parse, and every required
                /// field left unset, is reported -- all of them at once.
                #[allow(dead_code)]
                pub fn resolve(
                    cli: [<Raw $category:camel>],
                    scope: $crate::settings_file::ConfigScope<'_>,
                ) -> Result<Self, $crate::settings::SettingsError> {
                    let docs = $crate::settings_file::layers(scope);
                    let layers: Vec<&$crate::toml_edit::Item> =
                        docs.iter().flatten().map(|doc| doc.as_item()).collect();
                    Self::resolve_layers(cli, &layers)
                }

                pub fn resolve_layers(
                    cli: [<Raw $category:camel>],
                    layers: &[&$crate::toml_edit::Item],
                ) -> Result<Self, $crate::settings::SettingsError> {
                    let mut errors = Vec::new();
                    $(
                        let $field: Option<$ty> = match Self::[<resolve_ $field>](&cli, layers) {
                            Ok(Some(value)) => Some(value),
                            Ok(None) => {
                                let default: Option<$ty> = $crate::__settings_default!($($default)?);
                                if default.is_none() {
                                    errors.push($crate::settings::SettingError {
                                        setting: $crate::settings::env_name(
                                            stringify!($category),
                                            stringify!($field),
                                        ),
                                        problem: format!(
                                            "is required (or set [{}] {} in config.toml)",
                                            stringify!($category),
                                            stringify!($field),
                                        ),
                                    });
                                }
                                default
                            }
                            Err(e) => {
                                errors.push(e);
                                None
                            }
                        };
                    )+
                    if !errors.is_empty() {
                        return Err($crate::settings::SettingsError(errors));
                    }
                    Ok(Self { $( $field: $field.expect("no error means every field resolved"), )+ })
                }

                /// The env layer alone (legacy name, then convention name),
                /// as a raw struct -- for a caller that consults its own
                /// config source next.
                #[allow(dead_code)]
                pub fn env_layer() -> Result<[<Raw $category:camel>], $crate::settings::SettingsError> {
                    let mut errors = Vec::new();
                    let raw = [<Raw $category:camel>] {
                        $(
                            [<$category _ $field>]: match Self::[<env_ $field>]() {
                                Ok(value) => value,
                                Err(e) => {
                                    errors.push(e);
                                    None
                                }
                            },
                        )+
                    };
                    if errors.is_empty() { Ok(raw) } else { Err($crate::settings::SettingsError(errors)) }
                }

                $(
                    #[doc(hidden)]
                    fn [<env_ $field>]() -> Result<Option<$ty>, $crate::settings::SettingError> {
                        $(
                            if let Some(value) = $crate::settings::env_value::<$ty>($legacy)? {
                                return Ok(Some(value));
                            }
                        )?
                        $crate::settings::env_value::<$ty>(&$crate::settings::env_name(
                            stringify!($category),
                            stringify!($field),
                        ))
                    }

                    #[doc(hidden)]
                    fn [<resolve_ $field>](
                        cli: &[<Raw $category:camel>],
                        layers: &[&$crate::toml_edit::Item],
                    ) -> Result<Option<$ty>, $crate::settings::SettingError> {
                        if let Some(value) = cli.[<$category _ $field>].clone() {
                            return Ok(Some(value));
                        }
                        if let Some(value) = Self::[<env_ $field>]()? {
                            return Ok(Some(value));
                        }
                        for doc in layers {
                            if let Some(item) = doc
                                .get(stringify!($category))
                                .and_then(|section| section.get(stringify!($field)))
                            {
                                return <$ty as $crate::settings::FromTomlItem>::from_toml_item(item)
                                    .map(Some)
                                    .map_err(|problem| $crate::settings::SettingError {
                                        setting: format!(
                                            "[{}] {} in config.toml",
                                            stringify!($category),
                                            stringify!($field),
                                        ),
                                        problem,
                                    });
                            }
                        }
                        Ok(None)
                    }
                )+
            }
        }
    };
}
```

Keep the existing macro doc comment above `macro_rules! settings`. Add a paragraph on `= default` being optional and `#[legacy = "NAME"]`, plus the doc tests from Step 1.

- [ ] **Step 4: Run the tests.** Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib settings:: && cargo test -p infigraph-core --doc settings`. Expected: PASS. The rest of the workspace does not compile yet; Task 2 fixes the callers. Do not commit until Task 2 compiles.

### Task 2: Move every caller onto the strict API

**Files:**
- Modify, with the exact rewrite per site:
  - Plain `X::resolve(cli, scope)` → `X::resolve_or_default(cli, scope)` at: `crates/infigraph-mcp/src/proxy.rs:35`, `crates/infigraph-mcp/src/idle.rs:31,41`, `crates/infigraph-mcp/src/mcp_lock.rs:32`, `crates/infigraph-mcp/src/session_context.rs:441`, `crates/infigraph-docs/src/watch.rs:147`, `crates/infigraph-core/src/instances.rs:302`, `crates/infigraph-core/src/lockfile.rs:166`, `crates/infigraph-core/src/lib.rs:289`, `crates/infigraph-core/src/graph/store.rs:377`, `crates/infigraph-core/src/graph/store_util.rs:114,120,136`, `crates/infigraph-core/src/watchdog.rs:35`, `crates/infigraph-core/src/doctor.rs:591`, `crates/infigraph-core/src/daemon/mod.rs:75,828,1446,2430`, `crates/infigraph-core/src/review/llm.rs:285`, `crates/infigraph-core/src/watch/producer.rs:132`, `crates/infigraph-core/src/quarantine.rs:38`, `crates/infigraph-core/src/ignore_rules.rs:127` (`resolve_layers` → `resolve_layers_or_default`).
  - `legacy_env` seeding → a `#[legacy]` attribute on the field, and the call site's `cli` becomes `RawX::default()`:
    - `crates/infigraph-mcp/src/web/mod.rs:299-311`: `#[legacy = "INFIGRAPH_API_KEY"] api_key`
    - `crates/infigraph-core/src/instances.rs:58-74`: `#[legacy = "INFIGRAPH_ORG"] org`
    - `crates/infigraph-core/src/embed/mod.rs:89-101`: `#[legacy = "INFIGRAPH_MODEL_DIR"] model_dir`
    - `crates/infigraph-core/src/lib.rs:262-280`: `#[legacy = "INFIGRAPH_BIN"] bin`, `#[legacy = "INFIGRAPH_GH_HOST"] gh_host`, `#[legacy = "INFIGRAPH_GH_OWNER"] gh_owner`
    - `crates/infigraph-core/src/graph/mod.rs:89`: `#[legacy = "INFIGRAPH_DOC_HNSW_THRESHOLD"] doc_hnsw_threshold`; `crates/infigraph-docs/src/combined.rs:316-331` becomes `Graph::resolve_or_default(RawGraph::default(), ConfigScope::User).doc_hnsw_threshold as usize`, and its doc comment drops the seeding explanation.
    - `crates/infigraph-mcp/src/session_context.rs:21-47`: `#[legacy = "INFIGRAPH_COMPRESSION_LEVEL"] compression_level`, `#[legacy = "INFIGRAPH_ML_COMPRESSION"] ml_compression`, `#[legacy = "INFIGRAPH_DEDUP"] dedup`, `#[legacy = "INFIGRAPH_TOKEN_BUDGET"] token_budget`, `#[legacy = "INFIGRAPH_KOMPRESS_DIR"] kompress_dir`. `session_cli()` becomes:

```rust
/// The `session` group's env layer only -- `None` means "not set at this
/// layer", so callers can consult config.toml next. A value that does not
/// parse is reported and treated as unset.
fn session_cli() -> RawSession {
    Session::env_layer().unwrap_or_else(|e| {
        eprintln!("warning: {e}; ignoring the environment for session settings");
        RawSession::default()
    })
}
```
  - Test call sites of `resolve_layers` (`crates/infigraph-core/tests/graph_settings.rs:9`) gain `.unwrap()`.

**Interfaces:**
- Consumes: Task 1's `resolve_or_default`, `resolve_layers_or_default`, `env_layer`, `#[legacy]`.
- Produces: nothing new. After this task, no caller of `legacy_env`/`env_override` remains (both were removed in Task 1).

- [ ] **Step 1: Make the edits above.** Every legacy-seeded group keeps its existing test (for example `hnsw_threshold_honors_legacy_name_over_canonical_name` in `combined.rs`, and the registry/install tests). Those tests are this task's regression gate for precedence, so they must pass unmodified.
- [ ] **Step 2: Build the workspace.** Run: `cargo clippy --all-targets -- -D warnings`. Expected: clean. Any remaining `resolve(` whose result is used as a struct is a site missed above. Fix it the same way and list it in the commit message.
- [ ] **Step 3: Run the affected suites.** Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib -- --test-threads=2`, then `cargo test -p infigraph-core --test graph_settings --test registry_settings`, then `cargo test -p infigraph-docs`. Expected: PASS.
- [ ] **Step 4: Commit Tasks 1+2 together** (Task 1 alone does not compile the workspace):

```bash
git add crates/
git commit -m "feat(settings): strict resolve, optional defaults, #[legacy] names (#74)"
```

### Task 3: `settings_enum!` + `BackendChoice`

**Files:**
- Modify: `crates/infigraph-core/src/settings.rs` (add `settings_enum!` + tests)
- Modify: `crates/infigraph-core/src/lib.rs:192-253` (constants, backend group, `selected_backend`, `validated_backend`, `daemon_backend_selected`), `:410-440`, `:650-670`, `:700-715` (the three `init*` matches)
- Modify: `crates/infigraph-core/src/daemon/lifecycle.rs:47-49` (`is_remote_backend`)
- Modify: `crates/infigraph-core/tests/selected_backend.rs` (string comparisons → enum)

**Interfaces:**
- Produces:
  - `settings_enum!` generating `ALL: &'static [Self]`, `const fn as_str(self) -> &'static str`, `Display`, `FromStr<Err = String>`, `FromTomlItem`, `serde::Deserialize`
  - `pub enum BackendChoice { Kuzu, Daemon, Neo4j }`
  - `pub fn selected_backend() -> BackendChoice` (infallible, logged fallback)
  - `pub fn validated_backend() -> anyhow::Result<BackendChoice>` (strict)
  - `LOCAL_BACKEND`/`DAEMON_BACKEND` stay `&'static str`, now defined as `BackendChoice::X.as_str()`

- [ ] **Step 1: Write the failing tests.** In `settings.rs` tests:

```rust
    crate::settings_enum! {
        pub enum ToyColor {
            Red = "red",
            Blue = "blue",
        }
    }

    crate::settings! {
        toy_enum {
            color: ToyColor = ToyColor::Red,
        }
    }

    #[test]
    fn enum_parses_its_spelling_and_lists_valid_values_on_error() {
        assert_eq!("blue".parse::<ToyColor>(), Ok(ToyColor::Blue));
        assert_eq!(ToyColor::Blue.as_str(), "blue");
        assert_eq!(ToyColor::Blue.to_string(), "blue");
        let err = "green".parse::<ToyColor>().unwrap_err();
        assert!(err.contains("\"green\"") && err.contains("red, blue"), "{err}");
    }

    #[test]
    fn enum_field_resolves_from_env_toml_and_cli() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_ENUM_COLOR");
        let doc: toml_edit::DocumentMut = "[toy_enum]\ncolor = \"blue\"".parse().unwrap();
        let cli = RawToyEnum::parse_from(["test"]);
        assert_eq!(ToyEnum::resolve_layers(cli, &[doc.as_item()]).unwrap().color, ToyColor::Blue);
        let cli = RawToyEnum::parse_from(["test", "--toy-enum-color", "red"]);
        assert_eq!(ToyEnum::resolve_layers(cli, &[doc.as_item()]).unwrap().color, ToyColor::Red);
        std::env::set_var("INFIGRAPH_TOY_ENUM_COLOR", "green");
        let err = ToyEnum::resolve_layers(RawToyEnum::parse_from(["test"]), &[]).unwrap_err();
        std::env::remove_var("INFIGRAPH_TOY_ENUM_COLOR");
        assert!(err.to_string().contains("red, blue"), "{err}");
    }
```

  In `tests/selected_backend.rs`, replace the string asserts and add:

```rust
#[test]
fn defaults_to_daemon_when_unset() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert_eq!(infigraph_core::selected_backend(), infigraph_core::BackendChoice::Daemon);
    assert_eq!(infigraph_core::DAEMON_BACKEND, "daemon");
}

#[test]
fn reads_real_env_var_name_unchanged() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
    assert_eq!(infigraph_core::selected_backend(), infigraph_core::BackendChoice::Neo4j);
    std::env::remove_var("INFIGRAPH_BACKEND");
}

#[test]
fn a_typo_is_rejected_by_validation_naming_the_variable_and_choices() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "kuzuu");
    let err = infigraph_core::validated_backend().unwrap_err().to_string();
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert!(err.contains("INFIGRAPH_BACKEND") && err.contains("\"kuzuu\""), "{err}");
    assert!(err.contains("kuzu, daemon, neo4j"), "{err}");
}

#[test]
fn empty_backend_is_an_error_not_the_default() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_BACKEND", "");
    let result = infigraph_core::validated_backend();
    std::env::remove_var("INFIGRAPH_BACKEND");
    assert!(result.is_err(), "empty INFIGRAPH_BACKEND must not mean the default");
}

#[test]
fn init_refuses_an_unknown_backend_instead_of_opening_kuzu() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("INFIGRAPH_BACKEND", "kuzuu");
    let registry = infigraph_languages::bundled_registry().unwrap();
    let mut ig = infigraph_core::Infigraph::open(tmp.path(), registry).unwrap();
    let result = ig.init();
    std::env::set_var("INFIGRAPH_BACKEND", infigraph_core::LOCAL_BACKEND);
    assert!(result.is_err(), "unknown backend must not fall through to Kuzu");
    assert!(!tmp.path().join(".infigraph/graph").exists(), "no graph file may be created");
}
```

(Check `Infigraph::open`'s exact signature against `crates/infigraph-core/tests/backend_selection.rs`, which already builds one, and copy that construction verbatim.)

- [ ] **Step 2: Run them.** Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib settings:: && cargo test -p infigraph-core --test selected_backend -- --test-threads=1`. Expected: compile errors (`settings_enum!`, `BackendChoice`, `validated_backend` missing).

- [ ] **Step 3: Implement `settings_enum!`** in `settings.rs`:

```rust
/// Declares an enum-valued setting type: each variant with the exact
/// spelling it has in env vars, CLI flags and config.toml. Generates
/// `FromStr` (whose error lists every valid spelling), `as_str`,
/// `Display`, `FromTomlItem` and `Deserialize`, which is everything a
/// `settings!` field type needs.
#[macro_export]
macro_rules! settings_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $text, )+
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, String> {
                match s {
                    $( $text => Ok(Self::$variant), )+
                    other => Err(format!(
                        "unknown value {other:?}; expected one of: {}",
                        [$($text),+].join(", ")
                    )),
                }
            }
        }

        impl $crate::settings::FromTomlItem for $name {
            fn from_toml_item(item: &$crate::toml_edit::Item) -> Result<Self, String> {
                item.as_str()
                    .ok_or_else(|| "expected a string".to_string())?
                    .parse()
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = <String as serde::Deserialize>::deserialize(d)?;
                raw.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}
```

- [ ] **Step 4: Rewire the backend** in `lib.rs:192-253`. Keep the existing doc comments on `LOCAL_BACKEND`/`DAEMON_BACKEND`:

```rust
crate::settings_enum! {
    /// Which graph backend a process uses (`INFIGRAPH_BACKEND`).
    pub enum BackendChoice {
        /// Open the graph in this process.
        Kuzu = "kuzu",
        /// Route reads and writes through this project's daemon.
        Daemon = "daemon",
        /// Remote Neo4j + Postgres.
        Neo4j = "neo4j",
    }
}

pub const LOCAL_BACKEND: &str = BackendChoice::Kuzu.as_str();
pub const DAEMON_BACKEND: &str = BackendChoice::Daemon.as_str();

crate::settings! {
    backend {
        #[legacy = "INFIGRAPH_BACKEND"]
        selected: BackendChoice = BackendChoice::Daemon,
    }
}

/// The active backend, for callers that cannot fail: a value that does not
/// parse is logged and the default used. Anything that is about to open a
/// store goes through [`validated_backend`] instead, so a typo stops the
/// process rather than silently picking a backend (#74).
pub fn selected_backend() -> BackendChoice {
    Backend::resolve_or_default(RawBackend::default(), settings_file::ConfigScope::User).selected
}

/// The active backend, or a Config error naming the bad value and the valid
/// ones. Called at CLI/MCP startup and by every `Infigraph::init*`.
pub fn validated_backend() -> Result<BackendChoice> {
    Backend::resolve(RawBackend::default(), settings_file::ConfigScope::User)
        .map(|b| b.selected)
        .map_err(|e| anyhow::anyhow!("invalid backend setting: {e}"))
}

pub fn daemon_backend_selected() -> bool {
    selected_backend() == BackendChoice::Daemon
}
```

  Delete the `selected_backend` doc paragraph about seeding the CLI slot; `#[legacy]` replaces it. In each of the three `init*` functions, replace the leading `if daemon_backend_selected() { … }` + `let backend_env = selected_backend(); match backend_env.as_str() { … _ => kuzu }` with one `match validated_backend()? { BackendChoice::Daemon => { <the existing daemon block> } BackendChoice::Neo4j => { <the existing two neo4j cfg arms> } BackendChoice::Kuzu => { <the existing `_` arm> } }`. Keep each block's body verbatim. The match is exhaustive, so an unknown value can no longer reach Kuzu. In `lifecycle.rs:48`: `crate::selected_backend() == crate::BackendChoice::Neo4j`.

- [ ] **Step 5: Run.** Run: `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core --lib settings:: && cargo test -p infigraph-core --test selected_backend --test backend_selection --test watch_daemon -- --test-threads=1`, then `cargo clippy --all-targets -- -D warnings`. Expected: PASS / clean.
- [ ] **Step 6: Commit**: `feat(core): INFIGRAPH_BACKEND is a validated enum via settings_enum! (#74)`.

### Task 4: Validate at startup, log the choice, check Neo4j reachability

**Files:**
- Modify: `crates/infigraph-core/src/graph/neo4j_backend.rs` (add `ping`)
- Modify: `crates/infigraph-core/src/lib.rs` (add `check_backend_at_startup`)
- Modify: `crates/infigraph-cli/src/main.rs:1035-1052` (call it, skipping `doctor`)
- Modify: `crates/infigraph-mcp/src/main.rs::run` (call it, log via `mcp_log`)
- Modify: `crates/infigraph-core/src/doctor.rs` (a `backend setting` check)
- Test: `crates/infigraph-cli/tests/backend_validation.rs` (new)

**Interfaces:**
- Consumes: `validated_backend()` from Task 3.
- Produces: `pub fn check_backend_at_startup() -> anyhow::Result<BackendChoice>`, `Neo4jBackend::ping(&self) -> Result<()>`.

- [ ] **Step 1: Write the failing CLI test** `crates/infigraph-cli/tests/backend_validation.rs`:

```rust
//! #74: a typo'd INFIGRAPH_BACKEND stops the CLI with a Config error that
//! names the variable, rather than silently running some backend.

fn infigraph() -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_infigraph"));
    cmd.env_remove("INFIGRAPH_WATCH_DAEMON");
    cmd
}

#[test]
fn an_unknown_backend_fails_before_any_work() {
    let tmp = tempfile::tempdir().unwrap();
    let out = infigraph()
        .current_dir(tmp.path())
        .env("INFIGRAPH_BACKEND", "kuzuu")
        .args(["stats"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("INFIGRAPH_BACKEND") && stderr.contains("kuzu, daemon, neo4j"), "{stderr}");
    assert!(!tmp.path().join(".infigraph").exists(), "nothing may be created");
}

#[test]
fn doctor_runs_and_reports_an_invalid_backend() {
    let tmp = tempfile::tempdir().unwrap();
    let out = infigraph()
        .current_dir(tmp.path())
        .env("INFIGRAPH_BACKEND", "kuzuu")
        .args(["doctor"])
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("backend setting"), "doctor must run and report it: {text}");
    assert!(text.contains("kuzuu"), "{text}");
}
```

  (Check that `stats` is a real subcommand in `crates/infigraph-cli/src/main.rs`'s `Commands`. If not, use any read command, such as `status`.)

- [ ] **Step 2: Run it.** Run: `cargo build -p infigraph-cli && env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-cli --test backend_validation`. Expected: FAIL (the first test currently falls through to a daemon and succeeds or hangs, and doctor has no such check).

- [ ] **Step 3: Implement.** In `neo4j_backend.rs`:

```rust
    /// One round trip, so a wrong URI or a server that is down surfaces at
    /// startup instead of on the first query (#74).
    pub fn ping(&self) -> Result<()> {
        self.run_void("RETURN 1")
    }
```

In `lib.rs`:

```rust
/// Startup check for CLI and MCP (#74): the backend setting must parse, and
/// a remote backend must answer. Returns the choice so the caller can log it.
pub fn check_backend_at_startup() -> Result<BackendChoice> {
    let backend = validated_backend()?;
    #[cfg(feature = "neo4j")]
    if backend == BackendChoice::Neo4j {
        graph::Neo4jBackend::connect_from_env()?
            .ping()
            .context("INFIGRAPH_BACKEND=neo4j but Neo4j is not reachable")?;
    }
    Ok(backend)
}
```

In `crates/infigraph-cli/src/main.rs` after `let cli = Cli::parse();`:

```rust
    // #74: refuse a bad backend setting before any work -- except doctor,
    // whose job is to report exactly that.
    if !matches!(cli.command, Commands::Doctor { .. }) {
        infigraph_core::check_backend_at_startup()?;
    }
```

(Use the actual `Commands` variant name for doctor. `main` returns `Result<()>`, so `?` prints the error and exits non-zero.)

In `crates/infigraph-mcp/src/main.rs::run`, before serving:

```rust
    match infigraph_core::check_backend_at_startup() {
        Ok(backend) => mcp_log("INFO", &format!("backend: {backend}")),
        Err(e) => {
            mcp_log("ERROR", &format!("{e:#}"));
            eprintln!("infigraph-mcp: {e:#}");
            std::process::exit(2);
        }
    }
```

In `doctor.rs`, add a check in the same shape as the neighbouring ones (category `config`, name `backend setting`). It passes with `backend: <choice>` when `validated_backend()` is `Ok`, and fails with the error text plus the fix "set INFIGRAPH_BACKEND to kuzu, daemon or neo4j (or unset it)" otherwise. Register it where the other config checks are listed.

- [ ] **Step 4: Run.** Run: `cargo build -p infigraph-cli -p infigraph-mcp && env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-cli --test backend_validation`, then the full gates: `cargo clippy --all-targets -- -D warnings`; `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-core -- --test-threads=2`; `env -u INFIGRAPH_WATCH_DAEMON INFIGRAPH_BACKEND=kuzu cargo test -p infigraph-cli -p infigraph-mcp -p infigraph-docs -- --test-threads=2`. Expected: all PASS; report the pass/ignore counts.
- [ ] **Step 5: Commit**: `feat: validate the backend at startup, log it, ping Neo4j (#74)`.

### Task 5: Docs and close-out

**Files:**
- Modify: `docs/superpowers/specs/2026-08-31-settings-macro-design.md`: document optional defaults, `#[legacy]`, strict `resolve`, `resolve_or_default`, `settings_enum!`; mark the backend migration's seeding workaround superseded.
- Modify: `CLAUDE.md` and `AGENTS.md` "Reads do not open the stores" bullet: `backend.selected` is now a `BackendChoice`, and a bad value is a startup error.

- [ ] **Step 1:** Make the doc edits.
- [ ] **Step 2:** Commit `docs: strict settings and backend enum (#74)`, push `feat/hardening`, close #74 with the commit list.
- [ ] **Step 3:** File the follow-up issue this plan deliberately leaves out: "startup `check_all` for every settings group". Today only the backend is checked at startup; other groups warn and use defaults at runtime.
