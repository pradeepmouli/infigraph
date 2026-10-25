//! A registry fixture that lives on disk, apart from the developer's own.
//!
//! `sync_group_contracts` and `index_group` write through `Registry::update`,
//! which changes what is *stored*, so a group a test builds in memory has to
//! be stored first -- in a directory of this test binary's own.

use infigraph_core::multi::Registry;
use std::sync::OnceLock;

static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();

/// Store `fixture`'s repos and groups in this binary's private registry
/// (merging, so tests running side by side keep each other's entries) and
/// hand the fixture back.
pub fn stored(fixture: Registry) -> Registry {
    let home = HOME.get_or_init(|| tempfile::tempdir().expect("registry scratch dir"));
    std::env::set_var("INFIGRAPH_REGISTRY_HOME", home.path());
    Registry::update(|stored| {
        stored.repos.extend(fixture.repos.clone());
        stored.groups.extend(fixture.groups.clone());
        Ok(())
    })
    .expect("store the fixture registry");
    fixture
}
