//! Where `.infigraph/config.toml` lives, and in what order its layers
//! apply.
//!
//! Deliberately paths only, no parsing: the two readers parse the same file
//! differently and for good reason -- `watch::config` uses `toml_edit` so a
//! surgical per-key write cannot drop sections it has no schema for, while
//! `infigraph-mcp`'s `session_context` deserializes into typed structs. What
//! they must agree on is *which files count and which wins*, and that is
//! what lives here.
//!
//! They did not agree. `watch::config` read only the project file and never
//! `$HOME`; `session_context` walked up from the process cwd and then fell
//! back to `$HOME`. Worse than inconsistent, the fallback made the mere
//! *existence* of a project file -- not its contents -- decide whether any
//! user-level setting applied at all, and `infigraph watch disable` creates
//! a project file holding nothing but `[watch]`. Running it silently
//! stopped a user's global compression settings from applying to that
//! project.
//!
//! # The rule
//!
//! Two layers, **merged per key**, nearer wins:
//!
//! 1. the project's `<root>/.infigraph/config.toml`
//! 2. the user's `~/.infigraph/config.toml`
//!
//! Per key is the load-bearing part. A layer that says nothing about a key
//! must fall through to the one below rather than blanking it, which is
//! what separates a layer from a fallback.
//!
//! This also answers `watch::config`'s original objection to consulting
//! `$HOME` -- that a machine-wide value would apply "with no per-project
//! override". Under a layered read the override exists by construction: the
//! project layer wins every key it states.
//!
//! # Still open (#160)
//!
//! The layers are agreed; *how the project root is found* is not.
//! `watch::config` takes an explicit root, which is correct -- a daemon or
//! MCP server's cwd routinely differs from the project it serves.
//! `session_context` walks up from `current_dir()`, because it is called
//! from process startup with no root in hand. Threading a root there is
//! part of #160's remaining work, along with caching (these readers parse
//! fresh on every call).

use std::path::{Path, PathBuf};

/// The project layer: `<root>/.infigraph/config.toml`. Returned whether or
/// not it exists -- callers treat an unreadable file as "states nothing".
pub fn project_config_path(root: &Path) -> PathBuf {
    root.join(".infigraph").join("config.toml")
}

/// The user layer: `~/.infigraph/config.toml`, or `None` when there is no
/// home directory or no such file.
///
/// `USERPROFILE` is the Windows spelling of `HOME`.
pub fn user_config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let candidate = PathBuf::from(home).join(".infigraph").join("config.toml");
    candidate.exists().then_some(candidate)
}
