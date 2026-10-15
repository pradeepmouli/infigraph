//! A subtree restriction for the graph-wide analysis tools (#30).
//!
//! A [`PathScope`] is a project-relative path prefix, `crates/infigraph-core`
//! say, that limits an analysis to the files under it. There is one matcher so
//! every tool that takes a `path_prefix` agrees on what "under" means: whole
//! path components, never a bare string prefix (`crates/core` does not contain
//! `crates/core-extras/x.rs`).

use std::path::Path;

/// "Only the files under this prefix", or everything when no prefix was given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathScope {
    /// Normalized: `/` separators, no leading `./`, no trailing `/`; never empty.
    prefix: Option<String>,
    /// A shared graph (remote mode) stores every file as `org/repo/<path>`;
    /// the scope is still spelled relative to the repo, so matching drops this
    /// leading namespace from the stored path. See [`within`](Self::within).
    namespace: Option<String>,
}

impl PathScope {
    /// No restriction: [`contains`](Self::contains) is true for every file.
    pub fn everything() -> Self {
        Self::default()
    }

    /// The scope a caller's `path_prefix` argument names. `raw` is relative to
    /// `project_root`, or absolute and inside it (made relative here). `None`,
    /// an empty string and `.` all mean no restriction.
    pub fn new(project_root: &Path, raw: Option<&str>) -> Self {
        let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
            return Self::everything();
        };
        let candidate = Path::new(raw);
        let relative = if candidate.is_absolute() {
            candidate
                .strip_prefix(project_root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| raw.to_string())
        } else {
            raw.to_string()
        };
        let mut prefix = relative.replace('\\', "/");
        while let Some(rest) = prefix.strip_prefix("./") {
            prefix = rest.to_string();
        }
        let prefix = prefix.trim_end_matches('/');
        if prefix.is_empty() || prefix == "." {
            return Self::everything();
        }
        Self {
            prefix: Some(prefix.to_string()),
            namespace: None,
        }
    }

    /// This scope for a graph whose stored paths carry `namespace` (`org/repo`
    /// in a shared remote graph; `GraphBackend::repo_filter`). A file stored as
    /// `org/repo/src/a.rs` is then matched as `src/a.rs`, and `None` leaves
    /// stored paths as they are.
    pub fn within(mut self, namespace: Option<&str>) -> Self {
        self.namespace = namespace
            .map(|ns| ns.trim_matches('/').to_string())
            .filter(|ns| !ns.is_empty());
        self
    }

    /// The normalized prefix, `None` when unrestricted.
    pub fn prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// A name for this scope that is safe in a file name and unique to it: the
    /// prefix, readable, plus a digest of the exact prefix. The digest is what
    /// keeps `crates/foo-bar` and `crates/foo_bar`, which read the same once
    /// punctuation is flattened, from sharing one saved report.
    pub fn slug(&self) -> Option<String> {
        let prefix = self.prefix.as_deref()?;
        let readable: String = prefix
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let digest = crate::embed::fnv1a64(prefix.as_bytes()) as u32;
        Some(format!("{readable}-{digest:08x}"))
    }

    pub fn is_scoped(&self) -> bool {
        self.prefix.is_some()
    }

    /// Whether the project-relative `file` is the scope's prefix itself or lies
    /// under it.
    pub fn contains(&self, file: &str) -> bool {
        let Some(prefix) = &self.prefix else {
            return true;
        };
        let file = file.strip_prefix("./").unwrap_or(file);
        let file = self
            .namespace
            .as_deref()
            .and_then(|ns| file.strip_prefix(ns))
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(file);
        match file.strip_prefix(prefix.as_str()) {
            Some("") => true,
            Some(rest) => rest.starts_with('/') || rest.starts_with('\\'),
            None => false,
        }
    }

    /// `items` restricted to those whose file (as `file_of` names it) the
    /// scope contains. Order is kept.
    pub fn retain<T>(&self, items: Vec<T>, file_of: impl Fn(&T) -> &str) -> Vec<T> {
        if !self.is_scoped() {
            return items;
        }
        items
            .into_iter()
            .filter(|item| self.contains(file_of(item)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(raw: &str) -> PathScope {
        PathScope::new(Path::new("/proj"), Some(raw))
    }

    #[test]
    fn no_prefix_is_no_restriction() {
        for raw in [
            None,
            Some(""),
            Some("  "),
            Some("."),
            Some("./"),
            Some("/proj"),
        ] {
            let s = PathScope::new(Path::new("/proj"), raw);
            assert!(!s.is_scoped(), "{raw:?}");
            assert!(s.contains("anything/at/all.rs"), "{raw:?}");
        }
    }

    #[test]
    fn a_prefix_matches_whole_components_only() {
        let s = scope("crates/core");
        assert!(s.contains("crates/core/src/lib.rs"));
        assert!(s.contains("crates/core"), "the prefix itself, as a file");
        assert!(!s.contains("crates/core-extras/src/lib.rs"));
        assert!(!s.contains("crates/corelib.rs"));
        assert!(!s.contains("other/crates/core/src/lib.rs"));
    }

    #[test]
    fn spelling_of_the_prefix_does_not_matter() {
        let canonical = scope("crates/core");
        for raw in [
            "./crates/core",
            "crates/core/",
            "crates\\core",
            "/proj/crates/core",
        ] {
            assert_eq!(scope(raw), canonical, "{raw}");
        }
        assert_eq!(canonical.prefix(), Some("crates/core"));
    }

    #[test]
    fn an_absolute_path_outside_the_project_matches_nothing_in_it() {
        let s = scope("/elsewhere/crates/core");
        assert!(s.is_scoped());
        assert!(!s.contains("crates/core/src/lib.rs"));
    }

    #[test]
    fn a_shared_graphs_namespace_is_dropped_before_matching() {
        let s = scope("src").within(Some("org/repo"));
        assert!(s.contains("org/repo/src/a.rs"), "stored with its namespace");
        assert!(s.contains("src/a.rs"), "a path without one still matches");
        assert!(!s.contains("org/repo/tests/a.rs"));
        assert!(
            !s.contains("org/other/src/a.rs"),
            "another repo's namespace is not this repo's"
        );
        // Not a bare string prefix either: `org/repository/src` is another repo.
        assert!(!s.contains("org/repository/src/a.rs"));
        assert_eq!(scope("src").within(None), scope("src"));
    }

    #[test]
    fn slugs_of_scopes_that_differ_only_in_punctuation_differ() {
        let (a, b) = (scope("crates/foo-bar"), scope("crates/foo_bar"));
        let (a, b) = (a.slug().unwrap(), b.slug().unwrap());
        assert_ne!(a, b);
        assert!(a.starts_with("crates-foo-bar-") && b.starts_with("crates-foo-bar-"));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        assert_eq!(PathScope::everything().slug(), None);
        assert_eq!(scope("./crates/foo-bar/").slug().unwrap(), a);
    }

    #[test]
    fn retain_keeps_order_and_only_files_in_scope() {
        let files = vec!["a/x.rs", "b/y.rs", "a/z.rs", "ab/w.rs"];
        let kept = scope("a").retain(files.clone(), |f| f);
        assert_eq!(kept, ["a/x.rs", "a/z.rs"]);
        assert_eq!(PathScope::everything().retain(files.clone(), |f| f), files);
    }
}
