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
        assert_eq!(ToySettings::resolve(cli, Some(toml_item)).grace_secs, 42);
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
