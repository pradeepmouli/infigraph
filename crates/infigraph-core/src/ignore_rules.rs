//! Shared, .gitignore- and .infigraphignore-aware ignore rules used by
//! every directory walker and the file watcher, so a project convention
//! excluded via .gitignore (or .infigraphignore) is honored everywhere
//! consistently, instead of each call site maintaining its own hardcoded
//! directory-name list.
//!
//! `[index] include` in `.infigraph/config.toml` is the one escape hatch:
//! it names directories to index *despite* those rules, for a vendored
//! reference library worth having in the graph. It is deliberately the
//! only way in -- the safety list stays maximally protective by default
//! and the user names each exception -- and it is read here rather than
//! passed in, so all six walkers and the watcher honor it without a
//! signature change.

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;

use crate::settings::PathList;
use crate::settings_file::ConfigScope;

crate::settings! {
    index {
        include: PathList = PathList(Vec::new()),
    }
}

/// Directories always excluded, regardless of what any .gitignore or
/// .infigraphignore says. Union of every previously-independent hardcoded
/// list this module replaces (collect_files, the watcher, doc indexing,
/// grep search, security scanning) -- unifying them must not silently
/// reduce protection in a repo whose own .gitignore happens to be sparse.
pub const IGNORE_SAFETY_LIST: &[&str] = &[
    ".infigraph",
    ".git",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "target",
    "build",
    "dist",
    ".tox",
    "vendor",
    ".idea",
    ".mypy_cache",
    "coverage",
    ".pytest_cache",
];

fn is_safety_excluded(name: &str) -> bool {
    IGNORE_SAFETY_LIST.contains(&name)
}

/// The `[index] include` directories that fall inside a walk of `root`,
/// spelled as `root`-relative joins so they share `root`'s exact prefix --
/// every entry a walker yields is `root` joined with components, so
/// comparing against a differently-spelled (e.g. canonicalized) path would
/// silently never match.
///
/// Entries are resolved against the *project* root, not the walk root, so
/// one `config.toml` means the same thing whether the caller walks the
/// whole project or one subdirectory of it (`walk_and_search` does the
/// latter). An entry outside this particular walk is dropped: walking
/// `src/` must not drag in a reference library under `node_modules/`.
fn include_roots(root: &Path) -> Vec<PathBuf> {
    let project_root = crate::project::resolve_project_root(root);
    let settings = Index::resolve(RawIndex::default(), ConfigScope::Project(&project_root));
    if settings.include.0.is_empty() {
        return Vec::new();
    }
    let walk_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    settings
        .include
        .0
        .iter()
        .filter_map(|entry| match classify_include(&project_root, entry) {
            Ok(absolute) => Some(absolute),
            Err(problem) => {
                warn_about_unusable_include(entry, problem);
                None
            }
        })
        // Re-spell against this walk's root, and drop anything outside it:
        // walking `src/` must not drag in a library under `node_modules/`.
        // This is a scope check, not a mistake, so it warns about nothing.
        .filter_map(|absolute| {
            let relative = absolute.strip_prefix(&walk_root).ok()?;
            Some(root.join(relative))
        })
        .collect()
}

/// Why an `[index] include` entry cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncludeProblem {
    /// Absolute, or climbing out with `..`. Rejected rather than resolved:
    /// a project's config file must not be able to widen indexing past its
    /// own project.
    OutsideProject,
    /// Names nothing, or names a file. Almost always a typo or a path left
    /// behind by a dependency bump.
    NotADirectory,
}

/// Resolves one `[index] include` entry against `project_root`.
fn classify_include(project_root: &Path, entry: &str) -> Result<PathBuf, IncludeProblem> {
    let relative = Path::new(entry);
    if !relative.is_relative()
        || relative
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(IncludeProblem::OutsideProject);
    }
    let absolute = project_root.join(relative);
    if !absolute.is_dir() {
        return Err(IncludeProblem::NotADirectory);
    }
    Ok(absolute)
}

/// Says so, once per process per entry, when an include entry is unusable.
///
/// Silence would be the worst outcome here: an entry that resolves to
/// nothing looks exactly like the feature not working, and the walkers that
/// consult this run often enough that warning per call would be noise.
fn warn_about_unusable_include(entry: &str, problem: IncludeProblem) {
    static WARNED: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
        std::sync::Mutex::new(None);

    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if !warned
        .get_or_insert_with(std::collections::HashSet::new)
        .insert(entry.to_string())
    {
        return;
    }
    let reason = match problem {
        IncludeProblem::OutsideProject => "must be a path inside the project (no leading / or ..)",
        IncludeProblem::NotADirectory => "names no directory",
    };
    eprintln!("warning: [index] include entry {entry:?} {reason}; ignoring it");
}

/// Installs the shared entry filter on `builder`, plus each include root as
/// an additional walk root.
///
/// Adding them as roots is what defeats `.gitignore`: a re-admitted
/// directory is almost always ignored by the project's own ignore file too
/// (`node_modules/`), and gitignore semantics give no way to re-include a
/// path under an excluded directory. `parents(false)` is what stops an
/// added root from inheriting that rule from above it, and it is applied
/// *only* when there is something to include, so a project not using this
/// feature keeps inheriting an enclosing repository's `.gitignore` exactly
/// as before.
///
/// The depth check keeps a single directory from being walked twice. An
/// include root that the ordinary descent can also reach (one excluded by
/// the safety list alone, say) would otherwise be yielded once from above
/// and once as its own root, and duplicate entries mean duplicate indexing.
/// Reaching it at depth 0 means it *is* the added root; any greater depth
/// is the ordinary descent arriving, and that arrival is the one to drop.
fn apply_filter_and_includes(builder: &mut WalkBuilder, includes: Vec<PathBuf>) {
    if !includes.is_empty() {
        builder.parents(false);
        for include in &includes {
            builder.add(include);
        }
    }

    builder.filter_entry(move |entry| {
        if entry.depth() > 0 && includes.iter().any(|root| root == entry.path()) {
            return false;
        }
        !is_safety_excluded(&entry.file_name().to_string_lossy())
    });
}

/// A pre-configured `WalkBuilder` for `root`: respects `.gitignore`,
/// `.infigraphignore`, and the safety list above, minus any directory
/// `[index] include` opts back in. Callers may add further configuration
/// (e.g. `.max_depth`) before calling `.build()`.
pub fn walk_builder(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .require_git(false)
        .add_custom_ignore_filename(".infigraphignore");
    apply_filter_and_includes(&mut builder, include_roots(root));
    builder
}

/// Point-wise matcher for a single path (e.g. a file-watcher event), where
/// there's no directory tree to walk. Built from the same safety list and
/// the same `.gitignore`/`.infigraphignore` files `walk_builder` would
/// discover -- rebuild when those files may have changed (the watcher
/// rebuilds this on its periodic tick; see `run_write_coordinator`).
pub struct IgnoreMatcher {
    root: PathBuf,
    gitignore: Gitignore,
    /// `[index] include` roots, spelled against `root` exactly as the
    /// paths handed to `is_ignored` are, so `starts_with` can compare them
    /// without canonicalizing on every call.
    includes: Vec<PathBuf>,
    /// Rules from only those ignore files living at or below an include
    /// root. Inside an include root this replaces `gitignore`, which is the
    /// point-wise twin of the walker's `parents(false)`: the project-level
    /// `node_modules/` rule must not reach a re-admitted subtree, while a
    /// `.gitignore` *inside* that subtree still must. Matching the walker
    /// here is not cosmetic -- a matcher that admitted more than the walker
    /// would have the watcher mark files the indexer never stores, which is
    /// the orphaned-dirty-mark failure mode.
    inside_includes: Gitignore,
}

impl IgnoreMatcher {
    /// Discovers every `.gitignore`/`.infigraphignore` under `root`
    /// (skipping the safety list, same as `walk_builder`, so this never
    /// wastes time descending into e.g. `node_modules/` hunting for nested
    /// ignore files there -- nothing inside is ever relevant since the
    /// whole directory is always excluded, barring an `[index] include`
    /// root, which is walked for exactly that reason), then builds one
    /// matcher from all of them. `.hidden(false)` here (unlike `walk_builder`) because
    /// the ignore files themselves are dot-prefixed and must be visited as
    /// walk results to be found; `.git_ignore(true)` + `.require_git(false)`
    /// still prune any subtree an already-discovered ancestor `.gitignore`
    /// excludes (with `require_git(false)` allowing this even in non-git roots),
    /// so this stays proportional to directory count, not full file count.
    pub fn build(root: &Path) -> Self {
        let root = root.to_path_buf();
        let mut gi_builder = GitignoreBuilder::new(&root);

        let mut inside_builder = GitignoreBuilder::new(&root);

        let includes = include_roots(&root);
        let mut discovery = WalkBuilder::new(&root);
        discovery
            .hidden(false)
            .git_ignore(true)
            .require_git(false)
            .add_custom_ignore_filename(".infigraphignore");
        apply_filter_and_includes(&mut discovery, includes.clone());

        for result in discovery.build() {
            let Ok(entry) = result else { continue };
            let name = entry.file_name().to_string_lossy();
            if name != ".gitignore" && name != ".infigraphignore" {
                continue;
            }
            let _ = gi_builder.add(entry.path());
            if includes.iter().any(|root| entry.path().starts_with(root)) {
                let _ = inside_builder.add(entry.path());
            }
        }

        let gitignore = gi_builder.build().unwrap_or_else(|_| Gitignore::empty());
        let inside_includes = inside_builder
            .build()
            .unwrap_or_else(|_| Gitignore::empty());
        IgnoreMatcher {
            root,
            gitignore,
            includes,
            inside_includes,
        }
    }

    /// True if `path` should be excluded -- either via the safety list
    /// (checked against every path component, so a nested occurrence like
    /// `foo/node_modules/bar` is still caught) or via a discovered
    /// `.gitignore`/`.infigraphignore` rule. Inside an `[index] include`
    /// root both checks narrow to the part of the path below that root.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let include = self.includes.iter().find(|root| path.starts_with(root));

        // The safety list still applies *below* an include root -- opting a
        // reference library back in says nothing about its own vendored
        // `node_modules/` or `.git/`. Above the include root it is exactly
        // what the entry is opting out of, so only the tail is checked.
        let checked = match include {
            Some(root) => path.strip_prefix(root).unwrap_or(path),
            None => path,
        };
        if checked
            .components()
            .any(|c| is_safety_excluded(&c.as_os_str().to_string_lossy()))
        {
            return true;
        }
        // Strip root prefix to get relative path for gitignore matching.
        let rel_path = path.strip_prefix(&self.root).unwrap_or(path);
        let rules = if include.is_some() {
            &self.inside_includes
        } else {
            &self.gitignore
        };

        // Check if the path itself matches gitignore rules
        if rules.matched(rel_path, is_dir).is_ignore() {
            return true;
        }

        // Check if any parent directory matches gitignore rules (directories
        // like "scratchpad/" in .gitignore should exclude all descendants)
        let mut current = rel_path;
        while let Some(parent) = current.parent() {
            if parent == Path::new("") {
                break;
            }
            if rules.matched(parent, true).is_ignore() {
                return true;
            }
            current = parent;
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "scratchpad/\n*.log\n").unwrap();
        fs::create_dir_all(dir.path().join("scratchpad/wt-foo")).unwrap();
        fs::write(dir.path().join("scratchpad/wt-foo/README.md"), "# copy").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("debug.log"), "noisy").unwrap();
        fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        fs::write(dir.path().join("node_modules/pkg/index.js"), "//").unwrap();
        dir
    }

    #[test]
    fn walk_builder_skips_gitignored_scratchpad_and_safety_list() {
        let dir = make_fixture();
        let mut found = Vec::new();
        for result in walk_builder(dir.path()).build() {
            let entry = result.unwrap();
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                found.push(entry.path().to_path_buf());
            }
        }
        assert!(found.iter().any(|p| p.ends_with("src/main.rs")));
        assert!(
            !found
                .iter()
                .any(|p| p.to_string_lossy().contains("scratchpad")),
            "scratchpad/ is gitignored and must not be walked: {found:?}"
        );
        assert!(
            !found
                .iter()
                .any(|p| p.to_string_lossy().contains("node_modules")),
            "node_modules/ is in the safety list and must not be walked: {found:?}"
        );
        assert!(
            !found.iter().any(|p| p.ends_with("debug.log")),
            "*.log is gitignored and must not be walked: {found:?}"
        );
    }

    #[test]
    fn ignore_matcher_agrees_with_walk_builder() {
        let dir = make_fixture();
        let matcher = IgnoreMatcher::build(dir.path());

        assert!(!matcher.is_ignored(&dir.path().join("src/main.rs"), false));
        assert!(matcher.is_ignored(&dir.path().join("scratchpad/wt-foo/README.md"), false));
        assert!(matcher.is_ignored(&dir.path().join("scratchpad"), true));
        assert!(matcher.is_ignored(&dir.path().join("debug.log"), false));
        assert!(matcher.is_ignored(&dir.path().join("node_modules/pkg/index.js"), false));
    }

    #[test]
    fn infigraphignore_is_honored_like_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".infigraphignore"), "vendored/\n").unwrap();
        fs::create_dir_all(dir.path().join("vendored")).unwrap();
        fs::write(dir.path().join("vendored/lib.rs"), "// vendored").unwrap();
        fs::write(dir.path().join("real.rs"), "fn f() {}").unwrap();

        let matcher = IgnoreMatcher::build(dir.path());
        assert!(matcher.is_ignored(&dir.path().join("vendored/lib.rs"), false));
        assert!(!matcher.is_ignored(&dir.path().join("real.rs"), false));

        let mut found = Vec::new();
        for result in walk_builder(dir.path()).build() {
            let entry = result.unwrap();
            if entry.file_type().is_some_and(|ft| ft.is_file()) {
                found.push(entry.path().to_path_buf());
            }
        }
        assert!(!found
            .iter()
            .any(|p| p.to_string_lossy().contains("vendored")));
        assert!(found.iter().any(|p| p.ends_with("real.rs")));
    }

    #[test]
    fn ignore_matcher_works_in_git_initialized_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Initialize as a git directory
        let _ = std::fs::create_dir(dir.path().join(".git"));

        fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(dir.path().join("ignored")).unwrap();
        fs::write(dir.path().join("ignored/file.txt"), "ignored").unwrap();
        fs::write(dir.path().join("kept.txt"), "kept").unwrap();

        let matcher = IgnoreMatcher::build(dir.path());
        assert!(matcher.is_ignored(&dir.path().join("ignored/file.txt"), false));
        assert!(!matcher.is_ignored(&dir.path().join("kept.txt"), false));
    }

    /// A reference library vendored under a safety-listed directory, opted
    /// back in by name. Both exclusion mechanisms are live here on purpose:
    /// `node_modules` is on the safety list *and* in .gitignore, so a
    /// fixture missing either would pass against a half-built override.
    fn make_include_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
        fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
        fs::write(
            dir.path().join(".infigraph/config.toml"),
            "[index]\ninclude = [\"node_modules/reference-lib\"]\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("node_modules/reference-lib/src")).unwrap();
        fs::write(
            dir.path().join("node_modules/reference-lib/src/api.ts"),
            "export const x = 1;",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("node_modules/other-pkg")).unwrap();
        fs::write(dir.path().join("node_modules/other-pkg/index.js"), "//").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        dir
    }

    fn walked_files(root: &Path) -> Vec<String> {
        walk_builder(root)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
            .map(|entry| {
                entry
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn an_included_directory_is_walked_despite_the_safety_list_and_gitignore() {
        let dir = make_include_fixture();
        let found = walked_files(dir.path());
        assert!(
            found
                .iter()
                .any(|p| p == "node_modules/reference-lib/src/api.ts"),
            "[index] include must re-admit the named subtree: {found:?}"
        );
        assert!(
            found.iter().any(|p| p == "src/main.rs"),
            "ordinary source must still be walked: {found:?}"
        );
    }

    #[test]
    fn an_include_does_not_widen_to_its_safety_listed_parent() {
        let dir = make_include_fixture();
        let found = walked_files(dir.path());
        assert!(
            !found.iter().any(|p| p.contains("other-pkg")),
            "only the named subtree is re-admitted, not the whole of node_modules: {found:?}"
        );
    }

    #[test]
    fn the_ignore_matcher_agrees_with_the_walker_about_an_included_directory() {
        let dir = make_include_fixture();
        let matcher = IgnoreMatcher::build(dir.path());
        assert!(
            !matcher.is_ignored(
                &dir.path().join("node_modules/reference-lib/src/api.ts"),
                false
            ),
            "the watcher must see included files the walker indexes, or edits there never reindex"
        );
        assert!(
            matcher.is_ignored(&dir.path().join("node_modules/other-pkg/index.js"), false),
            "the watcher must still exclude the rest of node_modules"
        );
    }

    #[test]
    fn git_and_infigraph_stay_excluded_inside_an_included_directory() {
        let dir = make_include_fixture();
        let git_dir = dir.path().join("node_modules/reference-lib/.git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main").unwrap();
        let nested = dir
            .path()
            .join("node_modules/reference-lib/node_modules/dep");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("index.js"), "//").unwrap();

        let found = walked_files(dir.path());
        assert!(
            !found.iter().any(|p| p.contains("/.git/")),
            ".git is never indexable, include or not: {found:?}"
        );
        assert!(
            !found
                .iter()
                .any(|p| p.contains("reference-lib/node_modules")),
            "the safety list still applies below an include root: {found:?}"
        );

        let matcher = IgnoreMatcher::build(dir.path());
        assert!(matcher.is_ignored(&git_dir.join("HEAD"), false));
        assert!(matcher.is_ignored(&nested.join("index.js"), false));
    }

    /// The conditional half of the design: a project that declares no
    /// includes must behave exactly as it did before this feature, which
    /// includes still inheriting an enclosing repository's .gitignore.
    #[test]
    fn a_project_without_includes_still_inherits_an_outer_gitignore() {
        let outer = tempfile::tempdir().unwrap();
        fs::write(outer.path().join(".gitignore"), "secret.rs\n").unwrap();
        let root = outer.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/secret.rs"), "// excluded from above").unwrap();

        let found = walked_files(&root);
        assert!(found.iter().any(|p| p == "src/main.rs"), "{found:?}");
        assert!(
            !found.iter().any(|p| p.contains("secret.rs")),
            "an enclosing .gitignore must keep applying when no include is declared: {found:?}"
        );
    }

    /// An include drops the rules coming from *above* the named directory,
    /// which is the whole point -- and nothing else. Its own `.gitignore`
    /// is a rule from within, so it keeps applying, and the walker and the
    /// matcher have to agree about that or the watcher marks files the
    /// indexer skips.
    #[test]
    fn an_include_still_honors_a_gitignore_inside_it() {
        let dir = make_include_fixture();
        let lib = dir.path().join("node_modules/reference-lib");
        fs::write(lib.join(".gitignore"), "generated/\n").unwrap();
        fs::create_dir_all(lib.join("generated")).unwrap();
        fs::write(lib.join("generated/bundle.js"), "//").unwrap();

        let found = walked_files(dir.path());
        assert!(
            found
                .iter()
                .any(|p| p == "node_modules/reference-lib/src/api.ts"),
            "{found:?}"
        );
        assert!(
            !found.iter().any(|p| p.contains("generated/bundle.js")),
            "a .gitignore inside an included directory still applies: {found:?}"
        );

        let matcher = IgnoreMatcher::build(dir.path());
        assert!(
            matcher.is_ignored(&lib.join("generated/bundle.js"), false),
            "the matcher must agree with the walker about rules from within"
        );
        assert!(!matcher.is_ignored(&lib.join("src/api.ts"), false));
    }

    /// A misspelled or stale entry is the likeliest way to use this feature
    /// wrong, and the symptom -- nothing gets indexed -- looks exactly like
    /// the feature not working. Each rejection carries the reason so the
    /// caller can say which it was.
    #[test]
    fn an_include_entry_is_rejected_with_the_reason_it_is_unusable() {
        let dir = make_include_fixture();
        let root = dir.path();

        assert_eq!(
            classify_include(root, "node_modules/reference-lib"),
            Ok(root.join("node_modules/reference-lib")),
        );
        assert_eq!(
            classify_include(root, "node_modules/no-such-lib"),
            Err(IncludeProblem::NotADirectory),
        );
        assert_eq!(
            classify_include(root, "node_modules/reference-lib/src/api.ts"),
            Err(IncludeProblem::NotADirectory),
            "include names directories; a file entry is a mistake, not a one-file include"
        );
        assert_eq!(
            classify_include(root, "../elsewhere"),
            Err(IncludeProblem::OutsideProject),
        );
        assert_eq!(
            classify_include(root, "/etc"),
            Err(IncludeProblem::OutsideProject),
        );
    }
}
