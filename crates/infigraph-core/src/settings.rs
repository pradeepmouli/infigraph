//! Convention-driven settings groups: one macro wires a struct to CLI
//! (clap), env vars, and TOML config with zero per-field naming attributes.
//! See docs/superpowers/specs/2026-08-31-settings-macro-design.md.

/// One setting that could not be resolved: `setting` names where it came
/// from (`INFIGRAPH_X_Y`, or `[x] y in config.toml`), `problem` says why.
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

/// Prints `warning: {message}` to stderr the first time this process sees
/// `message`, and returns whether it printed. The fallback resolvers run in
/// daemon loops and on every MCP request, where the same bad value would
/// otherwise repeat its warning on every call.
pub fn warn_once(message: &str) -> bool {
    static SEEN: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.insert(message.to_string()) {
        return false;
    }
    eprintln!("warning: {message}");
    true
}

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
/// is unusable; a key that is absent never reaches this. Implemented per
/// concrete type a settings group actually uses -- add an impl the first
/// time a group needs a new field type.
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
        item.as_str()
            .map(str::to_string)
            .ok_or_else(|| "expected a string".to_string())
    }
}

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
    fn from_toml_item(item: &toml_edit::Item) -> Result<Self, String> {
        item.as_bool()
            .map(Toggle)
            .ok_or_else(|| "expected true or false".to_string())
    }
}

/// A settings-group list-of-paths field: zero or more root-relative paths.
/// TOML states it as an array of strings; the env layer has only a flat
/// string, so there it is comma-separated, with entries trimmed and empty
/// ones dropped. Dropping empties is not tidiness -- an empty entry used as
/// a path prefix would match every path.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
pub struct PathList(pub Vec<String>);

impl std::str::FromStr for PathList {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(PathList(
            s.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_string)
                .collect(),
        ))
    }
}

impl FromTomlItem for PathList {
    /// Anything but an array of strings is an error: a lone string is the
    /// likely mistake (`include = "vendor"`), and silently reading it as no
    /// entries would hide it.
    fn from_toml_item(item: &toml_edit::Item) -> Result<Self, String> {
        const EXPECTED: &str = "expected an array of strings";
        let array = item.as_array().ok_or(EXPECTED)?;
        let mut entries = Vec::new();
        for value in array.iter() {
            let entry = value.as_str().ok_or(EXPECTED)?.trim();
            if !entry.is_empty() {
                entries.push(entry.to_string());
            }
        }
        Ok(PathList(entries))
    }
}

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

/// Declares a settings group. `$category` (a single, possibly-underscored
/// identifier, e.g. `mcp_idle`) names the group for env var names
/// (`INFIGRAPH_{CATEGORY}_{FIELD}`), category-qualified CLI flags (via
/// `paste!`, so `--{category}-{field}` falls out of clap's own kebab-case
/// derivation with zero explicit `long = "..."` attributes), the TOML
/// section it reads from, *and* the generated struct's name (via `paste!`'s
/// `:camel` case conversion, e.g. `mcp_idle` -> `McpIdle`/`RawMcpIdle`) --
/// one identifier serves all four roles, so two settings groups that want
/// to share a common namespace (e.g. `mcp_idle` and `mcp_lock`, both under
/// "mcp") stay distinct simply by being different identifiers, without a
/// separate struct-name token. `$category` is an explicit token rather than
/// derived from `module_path!()` because `paste!` can only paste compile-time
/// tokens, and `module_path!()`'s value is a runtime string macro_rules!
/// cannot re-tokenize without a proc-macro -- see the spec's "Convention"
/// section for the full reasoning.
///
/// Each field is `name: Type = default` or, for a required setting, just
/// `name: Type`. A field may carry `#[legacy = "NAME"]` for a pre-macro env
/// var name that must keep working; it ranks just below the CLI.
/// Precedence: CLI > legacy name > `INFIGRAPH_{CATEGORY}_{FIELD}` > TOML
/// (nearest layer first) > declared default.
///
/// `resolve` is strict: a value that does not parse, or a required field
/// left unset, is an error, and every one in the group is reported. Only a
/// group whose every field declares a default also gets `impl Default` and
/// `resolve_or_default`, which logs a bad value and uses that field's
/// default -- a group with a required field has no default to fall back to:
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
#[macro_export]
macro_rules! settings {
    // Every field declares a default: the group also gets `Default` and the
    // logged-fallback resolvers. Tried first; a group with any required
    // field fails to match this arm and takes the next one.
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
                /// that run inside long-lived processes, where a config file
                /// edited mid-run must not take the process down.
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
                #[allow(dead_code)]
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
                                    $crate::settings::warn_once(&format!("{e}; using the default"));
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
                /// Every value that does not parse, and every required field
                /// left unset, is reported -- all of them at once.
                ///
                /// Loading the files here rather than taking a section
                /// parameter is the point of #160: when the section was a
                /// parameter, every production caller passed `None` and no
                /// config file ever reached a setting.
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

                /// [`resolve`](Self::resolve) over explicit config documents,
                /// nearest layer first. A key the nearer layer does not state
                /// falls through to the next one.
                #[allow(dead_code)]
                pub fn resolve_layers(
                    cli: [<Raw $category:camel>],
                    layers: &[&$crate::toml_edit::Item],
                ) -> Result<Self, $crate::settings::SettingsError> {
                    let mut errors = Vec::new();
                    $(
                        let $field: Option<$ty> = match Self::[<resolve_ $field>](&cli, layers) {
                            Ok(Some(value)) => Some(value),
                            Ok(None) => {
                                let default: Option<$ty> =
                                    $crate::__settings_default!($($default)?);
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
                    Ok(Self {
                        $( $field: $field.expect("no error means every field resolved"), )+
                    })
                }

                /// The env layer alone (legacy name, then convention name),
                /// as a raw struct -- for a caller that consults its own
                /// config source next. A value that does not parse is
                /// warned about and left unset; the group's other fields
                /// keep theirs.
                #[allow(dead_code)]
                pub fn env_layer() -> [<Raw $category:camel>] {
                    [<Raw $category:camel>] {
                        $(
                            [<$category _ $field>]: Self::[<env_ $field>]().unwrap_or_else(|e| {
                                $crate::settings::warn_once(&format!("{e}; ignoring it"));
                                None
                            }),
                        )+
                    }
                }

                $(
                    fn [<env_ $field>](
                    ) -> Result<Option<$ty>, $crate::settings::SettingError> {
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

/// Declares an enum-valued setting type: each variant with the exact
/// spelling it has in env vars, CLI flags and config.toml. Generates
/// `FromStr` (whose error lists every valid spelling), `as_str`, `Display`,
/// `FromTomlItem` and `Deserialize` -- everything a `settings!` field type
/// needs.
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
                        Self::ALL.iter().map(|v| v.as_str()).collect::<Vec<_>>().join(", ")
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

#[cfg(test)]
mod tests {
    use super::{PathList, Toggle};
    use clap::Parser;
    use std::sync::Mutex;

    /// Serializes tests that mutate process-global env vars -- `cargo test`
    /// runs unit tests in threads within one process, so two tests setting
    /// `INFIGRAPH_TOY_*` concurrently would race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    crate::settings! {
        toy_group {
            grace_secs: u64 = 300,
        }
    }

    #[test]
    fn resolves_hardcoded_default_when_nothing_else_set() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
        let cli = RawToyGroup::parse_from(["test"]);
        assert_eq!(ToyGroup::resolve_layers(cli, &[]).unwrap().grace_secs, 300);
    }

    #[test]
    fn env_overrides_hardcoded_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_GROUP_GRACE_SECS", "42");
        let cli = RawToyGroup::parse_from(["test"]);
        assert_eq!(ToyGroup::resolve_layers(cli, &[]).unwrap().grace_secs, 42);
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
    }

    #[test]
    fn toml_overrides_default_but_env_still_wins() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
        let doc: toml_edit::DocumentMut = "[toy_group]\ngrace_secs = 99".parse().unwrap();
        let layers = [doc.as_item()];

        let cli = RawToyGroup::parse_from(["test"]);
        assert_eq!(
            ToyGroup::resolve_layers(cli.clone(), &layers)
                .unwrap()
                .grace_secs,
            99
        );

        std::env::set_var("INFIGRAPH_TOY_GROUP_GRACE_SECS", "42");
        assert_eq!(
            ToyGroup::resolve_layers(cli, &layers).unwrap().grace_secs,
            42
        );
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
    }

    crate::settings! {
        toy_pair {
            near: u64 = 1,
            far: u64 = 2,
        }
    }

    /// #160: two layers merge per key. The nearer layer wins the key it
    /// states, and a key it is silent on falls through to the layer below
    /// rather than to the default -- that is what separates a layer from a
    /// fallback (see `settings_file`).
    #[test]
    fn the_nearer_layer_wins_per_key_and_its_silence_falls_through() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_PAIR_NEAR");
        std::env::remove_var("INFIGRAPH_TOY_PAIR_FAR");
        let project: toml_edit::DocumentMut = "[toy_pair]\nnear = 10".parse().unwrap();
        let user: toml_edit::DocumentMut = "[toy_pair]\nnear = 20\nfar = 30".parse().unwrap();

        let got = ToyPair::resolve_layers(
            RawToyPair::parse_from(["test"]),
            &[project.as_item(), user.as_item()],
        )
        .unwrap();
        assert_eq!(got, ToyPair { near: 10, far: 30 });
    }

    /// A group reads only its own `[category]` section: the same key under
    /// another group's header, or at the top level, is not its setting.
    #[test]
    fn a_group_reads_only_its_own_section() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
        let doc: toml_edit::DocumentMut = "grace_secs = 1\n[toy_other]\ngrace_secs = 2"
            .parse()
            .unwrap();
        let got =
            ToyGroup::resolve_layers(RawToyGroup::parse_from(["test"]), &[doc.as_item()]).unwrap();
        assert_eq!(got.grace_secs, 300);
    }

    /// End to end through the file loader: a real
    /// `<root>/.infigraph/config.toml` reaches `resolve`. Before #160 every
    /// production caller passed `None` here, so no file ever did.
    #[test]
    fn resolve_reads_the_project_config_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::settings_file::project_config_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[toy_group]\ngrace_secs = 99\n").unwrap();

        let cli = RawToyGroup::parse_from(["test"]);
        let scope = crate::settings_file::ConfigScope::Project(tmp.path());
        assert_eq!(ToyGroup::resolve(cli, scope).unwrap().grace_secs, 99);
    }

    #[test]
    fn cli_overrides_everything() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_GROUP_GRACE_SECS", "42");
        let cli = RawToyGroup::parse_from(["test", "--toy-group-grace-secs", "7"]);
        assert_eq!(ToyGroup::resolve_layers(cli, &[]).unwrap().grace_secs, 7);
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
    }

    #[test]
    fn cli_flag_is_category_qualified_not_bare() {
        let bare = RawToyGroup::try_parse_from(["test", "--grace-secs", "1"]);
        assert!(bare.is_err(), "bare --grace-secs must not be accepted");
        let qualified = RawToyGroup::parse_from(["test", "--toy-group-grace-secs", "1"]);
        assert_eq!(qualified.toy_group_grace_secs, Some(1));
    }

    // Two settings groups sharing a common namespace prefix ("toy_a"/"toy_b")
    // must not collide -- this is the whole point of folding category and
    // struct name into one identifier (see idle.rs's "mcp_idle" vs
    // mcp_lock.rs's "mcp_lock" for the real, shipped case this covers).
    crate::settings! {
        toy_a {
            value: u64 = 1,
        }
    }
    crate::settings! {
        toy_b {
            value: u64 = 2,
        }
    }

    #[test]
    fn two_groups_sharing_a_namespace_prefix_do_not_collide() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_A_VALUE");
        std::env::remove_var("INFIGRAPH_TOY_B_VALUE");
        let a = ToyA::resolve_layers(RawToyA::parse_from(["test"]), &[]).unwrap();
        let b = ToyB::resolve_layers(RawToyB::parse_from(["test"]), &[]).unwrap();
        assert_eq!(a.value, 1);
        assert_eq!(b.value, 2);
    }

    crate::settings! {
        toy_str {
            name: String = "default".to_string(),
        }
    }

    #[test]
    fn string_field_resolves_from_toml() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_STR_NAME");
        let doc: toml_edit::DocumentMut = "[toy_str]\nname = \"from-toml\"".parse().unwrap();
        let cli = RawToyStr::parse_from(["test"]);
        assert_eq!(
            ToyStr::resolve_layers(cli, &[doc.as_item()]).unwrap().name,
            "from-toml"
        );
    }

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
            ToyToggle::resolve_layers(cli, &[]).unwrap().flag.0,
            "\"1\" must be treated as true"
        );

        std::env::set_var("INFIGRAPH_TOY_TOGGLE_FLAG", "0");
        let cli = RawToyToggle::parse_from(["test"]);
        assert!(
            !ToyToggle::resolve_layers(cli, &[]).unwrap().flag.0,
            "\"0\" must be treated as false"
        );

        std::env::set_var("INFIGRAPH_TOY_TOGGLE_FLAG", "false");
        let cli = RawToyToggle::parse_from(["test"]);
        assert!(
            !ToyToggle::resolve_layers(cli, &[]).unwrap().flag.0,
            "\"false\" (any case) must be treated as false"
        );

        std::env::remove_var("INFIGRAPH_TOY_TOGGLE_FLAG");
        let cli = RawToyToggle::parse_from(["test"]);
        assert!(
            ToyToggle::resolve_layers(cli, &[]).unwrap().flag.0,
            "unset must fall through to the hardcoded default (true)"
        );
    }

    crate::settings! {
        toy_paths {
            include: PathList = PathList(Vec::new()),
        }
    }

    #[test]
    fn path_list_field_resolves_from_a_toml_array() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_PATHS_INCLUDE");
        let doc: toml_edit::DocumentMut =
            "[toy_paths]\ninclude = [\"node_modules/lib\", \"vendor/sdk\"]"
                .parse()
                .unwrap();
        let cli = RawToyPaths::parse_from(["test"]);
        assert_eq!(
            ToyPaths::resolve_layers(cli, &[doc.as_item()])
                .unwrap()
                .include,
            PathList(vec![
                "node_modules/lib".to_string(),
                "vendor/sdk".to_string()
            ]),
        );
    }

    /// The env layer has only a flat string to work with, so a list needs a
    /// separator. Comma, and entries are trimmed -- an env var written with
    /// spaces after the commas is the obvious way to get this wrong.
    #[test]
    fn path_list_parses_a_comma_separated_env_var() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(
            "INFIGRAPH_TOY_PATHS_INCLUDE",
            "node_modules/lib, vendor/sdk",
        );
        let cli = RawToyPaths::parse_from(["test"]);
        let got = ToyPaths::resolve_layers(cli, &[]).unwrap().include;
        std::env::remove_var("INFIGRAPH_TOY_PATHS_INCLUDE");
        assert_eq!(
            got,
            PathList(vec![
                "node_modules/lib".to_string(),
                "vendor/sdk".to_string()
            ]),
        );
    }

    /// An empty env var means "no entries", not one empty entry -- an empty
    /// path would otherwise match every path as a prefix.
    #[test]
    fn path_list_treats_an_empty_env_var_as_no_entries() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_PATHS_INCLUDE", "");
        let cli = RawToyPaths::parse_from(["test"]);
        let got = ToyPaths::resolve_layers(cli, &[]).unwrap().include;
        std::env::remove_var("INFIGRAPH_TOY_PATHS_INCLUDE");
        assert_eq!(got, PathList(Vec::new()));
    }

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
        assert!(
            ToyPaths::resolve_layers(RawToyPaths::parse_from(["test"]), &[doc.as_item()]).is_err()
        );
    }

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
        assert!(
            err.contains("\"green\"") && err.contains("red, blue"),
            "{err}"
        );
    }

    #[test]
    fn enum_field_resolves_from_env_toml_and_cli() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_ENUM_COLOR");
        let doc: toml_edit::DocumentMut = "[toy_enum]\ncolor = \"blue\"".parse().unwrap();
        let cli = RawToyEnum::parse_from(["test"]);
        assert_eq!(
            ToyEnum::resolve_layers(cli, &[doc.as_item()])
                .unwrap()
                .color,
            ToyColor::Blue
        );
        let cli = RawToyEnum::parse_from(["test", "--toy-enum-color", "red"]);
        assert_eq!(
            ToyEnum::resolve_layers(cli, &[doc.as_item()])
                .unwrap()
                .color,
            ToyColor::Red
        );
        std::env::set_var("INFIGRAPH_TOY_ENUM_COLOR", "green");
        let err = ToyEnum::resolve_layers(RawToyEnum::parse_from(["test"]), &[]).unwrap_err();
        std::env::remove_var("INFIGRAPH_TOY_ENUM_COLOR");
        assert!(err.to_string().contains("red, blue"), "{err}");
    }

    /// Review fix: one bad variable must not take the group's good ones
    /// with it -- `session`'s five legacy names used to all vanish because
    /// `INFIGRAPH_TOKEN_BUDGET` alone did not parse.
    #[test]
    fn env_layer_drops_only_the_bad_field() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_PAIR_NEAR", "x");
        std::env::set_var("INFIGRAPH_TOY_PAIR_FAR", "20");
        let raw = ToyPair::env_layer();
        std::env::remove_var("INFIGRAPH_TOY_PAIR_NEAR");
        std::env::remove_var("INFIGRAPH_TOY_PAIR_FAR");
        assert_eq!(raw.toy_pair_near, None);
        assert_eq!(raw.toy_pair_far, Some(20));
    }

    /// Review fix: `resolve_or_default` runs in daemon loops and on every
    /// MCP request, so the same bad value must warn once, not every call.
    #[test]
    fn warn_once_prints_each_message_once() {
        let message = "warn_once_prints_each_message_once: unique message";
        assert!(super::warn_once(message));
        assert!(!super::warn_once(message));
        assert!(super::warn_once(
            "warn_once_prints_each_message_once: another"
        ));
    }
}
