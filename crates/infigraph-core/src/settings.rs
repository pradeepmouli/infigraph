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

impl FromTomlItem for String {
    fn from_toml_item(item: &toml_edit::Item) -> Option<Self> {
        item.as_str().map(str::to_string)
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
    fn from_toml_item(item: &toml_edit::Item) -> Option<Self> {
        item.as_bool().map(Toggle)
    }
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
#[macro_export]
macro_rules! settings {
    (
        $category:ident {
            $( $field:ident : $ty:ty = $default:expr ),+ $(,)?
        }
    ) => {
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
                /// Resolves this group's settings: CLI > env > TOML > default,
                /// per field. `toml_section` is this group's own section
                /// (e.g. `doc.get("mcp_idle")`), or `None` if absent/not
                /// consulted.
                pub fn resolve(
                    cli: [<Raw $category:camel>],
                    toml_section: Option<&$crate::toml_edit::Item>,
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
    use super::Toggle;
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
        assert_eq!(ToyGroup::resolve(cli, None).grace_secs, 300);
    }

    #[test]
    fn env_overrides_hardcoded_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_GROUP_GRACE_SECS", "42");
        let cli = RawToyGroup::parse_from(["test"]);
        assert_eq!(ToyGroup::resolve(cli, None).grace_secs, 42);
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
    }

    #[test]
    fn toml_overrides_default_but_env_still_wins() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
        let doc: toml_edit::DocumentMut = "grace_secs = 99".parse().unwrap();
        let toml_item = doc.as_item();

        let cli = RawToyGroup::parse_from(["test"]);
        assert_eq!(
            ToyGroup::resolve(cli.clone(), Some(toml_item)).grace_secs,
            99
        );

        std::env::set_var("INFIGRAPH_TOY_GROUP_GRACE_SECS", "42");
        assert_eq!(ToyGroup::resolve(cli, Some(toml_item)).grace_secs, 42);
        std::env::remove_var("INFIGRAPH_TOY_GROUP_GRACE_SECS");
    }

    #[test]
    fn cli_overrides_everything() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("INFIGRAPH_TOY_GROUP_GRACE_SECS", "42");
        let cli = RawToyGroup::parse_from(["test", "--toy-group-grace-secs", "7"]);
        assert_eq!(ToyGroup::resolve(cli, None).grace_secs, 7);
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
        let a = ToyA::resolve(RawToyA::parse_from(["test"]), None);
        let b = ToyB::resolve(RawToyB::parse_from(["test"]), None);
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
        let doc: toml_edit::DocumentMut = r#"name = "from-toml""#.parse().unwrap();
        let toml_item = doc.as_item();
        let cli = RawToyStr::parse_from(["test"]);
        assert_eq!(ToyStr::resolve(cli, Some(toml_item)).name, "from-toml");
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
}
