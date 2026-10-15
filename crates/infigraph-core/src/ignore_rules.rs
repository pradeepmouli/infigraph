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

// `[ignore] code` / `[ignore] docs`: gitignore-style patterns that apply to
// ONE indexing domain, on top of everything every domain already skips.
// They only ever subtract, so unlike `[index] include` they cannot name
// anything outside the project and have no `Trust` rule.
//
// The layered read is shared with `include` (`layered_entries`), so the env
// layer *replaces* the config files here exactly as it does there --
// `INFIGRAPH_IGNORE_CODE` / `INFIGRAPH_IGNORE_DOCS` therefore make the walk
// MORE permissive than the files alone, by silencing the project's and the
// user's own lists. A value is comma-separated (`PathList`), so a pattern
// containing a comma can be stated in TOML only.
crate::settings! {
    ignore {
        code: PathList = PathList(Vec::new()),
        docs: PathList = PathList(Vec::new()),
    }
}

/// How often a long-lived watcher rebuilds its `IgnoreMatcher`, so an edit
/// to `.gitignore`, `.infigraphignore` or `[ignore]` reaches it without a
/// restart. Both watchers use it as their floor: the code producer when its
/// caller has no cadence of its own, the document watcher always.
pub const MATCHER_REBUILD_SECS: u64 = 300;

/// Which indexer a walk or a watcher event is for. Every file source names
/// its own: the safety list, `.gitignore`, `.infigraphignore` and
/// `[index] include` are the same for both, and `[ignore] <domain>` adds
/// the patterns for that one.
///
/// There is deliberately no domain-less entry point: a walker that did not
/// have to say what it is walking would silently skip its domain's rules,
/// which is the mismatch between walker and watcher (#189) in a new place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// The code graph: the indexer, text search, security scan and the
    /// code watcher.
    Code,
    /// The document index and its watcher.
    Docs,
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
/// Which config layer an `[index] include` entry came from. Only a layer
/// the user themself controls may name a path outside the project: their
/// own `~/.infigraph/config.toml` can point at a dependency's source in the
/// cargo registry, while a config file inside a repository they merely
/// cloned must not be able to name anything at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trust {
    /// A project's own `.infigraph/config.toml`.
    Project,
    /// `~/.infigraph/config.toml`, or this process's environment.
    User,
}

impl Trust {
    fn may_leave_the_project(self) -> bool {
        matches!(self, Trust::User)
    }
}

/// The include roots for a walk of `root`, plus the rules that still apply
/// inside them.
struct Includes {
    /// Spelled as `root`-relative joins so they share `root`'s exact prefix
    /// -- every entry a walker yields is `root` joined with components, so
    /// comparing against a differently-spelled (e.g. canonicalized) path
    /// would silently never match.
    roots: Vec<PathBuf>,
    /// The project's own `.infigraphignore`, matched against the path
    /// *relative to* an include root.
    ///
    /// This is the seam between the two ignore files. `.gitignore` says
    /// "not under source control", which is exactly what an include is
    /// overriding. `.infigraphignore` says "not worth indexing", which an
    /// include has no business overriding. Matching the relative tail is
    /// what lets both hold at once: `node_modules/` no longer matches
    /// (so the include works), while `src/parser.c` still does (so
    /// megabytes of generated parser tables stay out).
    tail_rules: std::sync::Arc<Gitignore>,
}

/// Every entry of one path-list setting, paired with the trust of the layer
/// that stated it.
///
/// Shared by `[index] include` and `[ignore] <domain>`, so the two cannot
/// drift apart on how layers combine. `from_env` is that setting's env
/// layer and `in_layer` reads it out of one config document.
///
/// The layers are read separately rather than through the group's
/// `resolve` because merging them would lose exactly the provenance
/// `Trust` needs. They also union rather than override, unlike a scalar
/// setting: a user saying "always index this dependency" and a project
/// saying "index this vendored library" are both true at once, and letting
/// either silence the other would serve nobody. The environment is the
/// exception, and replaces the files entirely -- it is this process's own,
/// so it is trusted, and it keeps the macro's usual precedence.
fn layered_entries(
    project_root: &Path,
    from_env: Option<PathList>,
    in_layer: impl Fn(&toml_edit::Item) -> PathList,
) -> Vec<(String, Trust)> {
    if let Some(from_env) = from_env {
        return from_env.0.into_iter().map(|e| (e, Trust::User)).collect();
    }

    let docs = crate::settings_file::layers(ConfigScope::Project(project_root));
    let [project, user] = docs;
    let entries_of = |doc: Option<std::sync::Arc<toml_edit::DocumentMut>>, trust: Trust| {
        doc.map(|doc| in_layer(doc.as_item()).0)
            .unwrap_or_default()
            .into_iter()
            .map(move |entry| (entry, trust))
    };

    let mut all: Vec<(String, Trust)> = entries_of(user, Trust::User).collect();
    for entry in entries_of(project, Trust::Project) {
        if !all.iter().any(|(existing, _)| *existing == entry.0) {
            all.push(entry);
        }
    }
    all
}

/// Every configured `[index] include` entry, paired with the trust of the
/// layer that stated it.
fn configured_includes(project_root: &Path) -> Vec<(String, Trust)> {
    layered_entries(project_root, Index::env_layer().index_include, |item| {
        Index::resolve_layers_or_default(RawIndex::default(), &[item]).include
    })
}

fn include_roots(root: &Path, project_root: &Path) -> Includes {
    let configured = configured_includes(project_root);
    if configured.is_empty() {
        return Includes {
            roots: Vec::new(),
            tail_rules: std::sync::Arc::new(Gitignore::empty()),
        };
    }
    let walk_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let roots = configured
        .iter()
        .filter_map(
            |(entry, trust)| match classify_include(project_root, entry, *trust) {
                Ok(absolute) => Some(absolute),
                Err(problem) => {
                    warn_about_unusable_include(entry, problem);
                    None
                }
            },
        )
        // Re-spell against this walk's root, and drop anything outside it:
        // walking `src/` must not drag in a library under `node_modules/`.
        // This is a scope check, not a mistake, so it warns about nothing.
        .filter_map(|absolute| {
            let relative = absolute.strip_prefix(&walk_root).ok()?;
            Some(root.join(relative))
        })
        .collect();

    let mut tail = GitignoreBuilder::new(project_root);
    let _ = tail.add(project_root.join(".infigraphignore"));
    Includes {
        roots,
        tail_rules: std::sync::Arc::new(tail.build().unwrap_or_else(|_| Gitignore::empty())),
    }
}

/// Why an `[index] include` entry cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncludeProblem {
    /// Points outside the project, from a layer not allowed to do that.
    OutsideProject,
    /// Names nothing, or names a file. Almost always a typo or a path left
    /// behind by a dependency bump.
    NotADirectory,
}

/// Resolves one `[index] include` entry against `project_root`.
fn classify_include(
    project_root: &Path,
    entry: &str,
    trust: Trust,
) -> Result<PathBuf, IncludeProblem> {
    let relative = Path::new(entry);
    let leaves_the_project = relative.is_absolute()
        || relative
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    if leaves_the_project && !trust.may_leave_the_project() {
        return Err(IncludeProblem::OutsideProject);
    }
    // An absolute entry replaces the base, which is what makes a trusted
    // absolute path resolve to itself.
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
    let reason = match problem {
        IncludeProblem::OutsideProject => {
            "must be inside the project (an absolute path or `..` is allowed              only from ~/.infigraph/config.toml)"
        }
        IncludeProblem::NotADirectory => "names no directory",
    };
    warn_once(
        format!("include {entry}"),
        format!("warning: [index] include entry {entry:?} {reason}; ignoring it"),
    );
}

/// Prints `message` the first time `key` is seen in this process. The
/// walkers that consult these settings run often enough that warning per
/// call would be noise.
fn warn_once(key: String, message: String) {
    static WARNED: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
        std::sync::Mutex::new(None);

    let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    if warned
        .get_or_insert_with(std::collections::HashSet::new)
        .insert(key)
    {
        eprintln!("{message}");
    }
}

/// The `[ignore] <domain>` patterns for one walk, as one gitignore matcher
/// anchored at the project root.
///
/// It is consulted exactly where the project's `.infigraphignore` tail
/// rules are, by both the walker's `filter_entry` and
/// `IgnoreMatcher::is_ignored`, so the two cannot disagree about a file
/// (#189). A pattern is gitignore syntax and nothing else: `/docs/`
/// anchors at the project root in every layer, `!` re-includes only within
/// the same list (it cannot re-admit what `.gitignore`, the safety list or
/// `.infigraphignore` excluded -- that is `[index] include`'s job), and a
/// pattern with a `..` component can never match inside the project, so it
/// is warned about and dropped.
struct DomainRules {
    rules: Gitignore,
    /// The walk root exactly as the walker or the watcher spells it.
    walk_root: PathBuf,
    /// Where `walk_root` sits below the project root, so a walk of one
    /// subdirectory still anchors `/docs/` at the project.
    prefix: PathBuf,
}

impl DomainRules {
    fn for_walk(root: &Path, project_root: &Path, domain: Domain) -> Self {
        let mut builder = GitignoreBuilder::new(project_root);
        for (pattern, _) in configured_domain_patterns(project_root, domain) {
            let leaves_the_project = Path::new(&pattern)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
            if leaves_the_project {
                warn_once(
                    format!("ignore {pattern}"),
                    format!(
                        "warning: [ignore] pattern {pattern:?} has a `..` component and can \
                         never match inside the project; ignoring it"
                    ),
                );
                continue;
            }
            if let Err(e) = builder.add_line(None, &pattern) {
                warn_once(
                    format!("ignore {pattern}"),
                    format!("warning: [ignore] pattern {pattern:?} is not valid: {e}; ignoring it"),
                );
            }
        }
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let project_canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        DomainRules {
            rules: builder.build().unwrap_or_else(|_| Gitignore::empty()),
            walk_root: root.to_path_buf(),
            prefix: canonical
                .strip_prefix(&project_canonical)
                .map(Path::to_path_buf)
                .unwrap_or_default(),
        }
    }

    /// True when a domain pattern excludes `path`, or any directory above
    /// it. Inside an `[index] include` root (`include`) the pattern is
    /// matched against the tail below that root, exactly as the project's
    /// `.infigraphignore` is: a re-admitted library is still full of
    /// generated files the project did not want, and a project-relative
    /// path would mean nothing there.
    fn ignores(&self, path: &Path, is_dir: bool, include: Option<&Path>) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let relative = match include {
            Some(include_root) => match path.strip_prefix(include_root) {
                Ok(tail) => tail.to_path_buf(),
                Err(_) => return false,
            },
            None => match path.strip_prefix(&self.walk_root) {
                Ok(below) => self.prefix.join(below),
                Err(_) => return false,
            },
        };
        if relative.as_os_str().is_empty() {
            return false;
        }
        self.rules
            .matched_path_or_any_parents(&relative, is_dir)
            .is_ignore()
    }
}

/// The `[ignore]` patterns for `domain`, from every layer.
fn configured_domain_patterns(project_root: &Path, domain: Domain) -> Vec<(String, Trust)> {
    let field = |ignore: Ignore| match domain {
        Domain::Code => ignore.code,
        Domain::Docs => ignore.docs,
    };
    let from_env = {
        let env = Ignore::env_layer();
        match domain {
            Domain::Code => env.ignore_code,
            Domain::Docs => env.ignore_docs,
        }
    };
    layered_entries(project_root, from_env, |item| {
        field(Ignore::resolve_layers_or_default(
            RawIgnore::default(),
            &[item],
        ))
    })
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
fn apply_filter_and_includes(
    builder: &mut WalkBuilder,
    includes: Includes,
    domain_rules: DomainRules,
) {
    let Includes { roots, tail_rules } = includes;
    if !roots.is_empty() {
        builder.parents(false);
        for include in &roots {
            builder.add(include);
        }
    }

    builder.filter_entry(move |entry| {
        let path = entry.path();
        if entry.depth() > 0 && roots.iter().any(|root| root == path) {
            return false;
        }
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        let include = roots.iter().find(|root| path.starts_with(root));
        if let Some(root) = include {
            let tail = path.strip_prefix(root).unwrap_or(path);
            if !tail.as_os_str().is_empty() && tail_rules.matched(tail, is_dir).is_ignore() {
                return false;
            }
        }
        if entry.depth() > 0 && domain_rules.ignores(path, is_dir, include.map(PathBuf::as_path)) {
            return false;
        }
        !is_safety_excluded(&entry.file_name().to_string_lossy())
    });
}

/// A pre-configured `WalkBuilder` for `root`: respects `.gitignore`,
/// `.infigraphignore`, and the safety list above, minus any directory
/// `[index] include` opts back in, plus the `[ignore]` patterns of
/// `domain`. Callers may add further configuration (e.g. `.max_depth`)
/// before calling `.build()`.
pub fn walk_builder(root: &Path, domain: Domain) -> WalkBuilder {
    let project_root = crate::project::resolve_project_root(root);
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .require_git(false)
        .add_custom_ignore_filename(".infigraphignore");
    apply_filter_and_includes(
        &mut builder,
        include_roots(root, &project_root),
        DomainRules::for_walk(root, &project_root, domain),
    );
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
    /// The project's `.infigraphignore`, matched against the path relative
    /// to an include root -- the point-wise twin of the walker's tail
    /// check, so the watcher does not mark a generated file the indexer
    /// skips.
    tail_rules: std::sync::Arc<Gitignore>,
    /// Rules from only those ignore files living at or below an include
    /// root. Inside an include root this replaces `gitignore`, which is the
    /// point-wise twin of the walker's `parents(false)`: the project-level
    /// `node_modules/` rule must not reach a re-admitted subtree, while a
    /// `.gitignore` *inside* that subtree still must. Matching the walker
    /// here is not cosmetic -- a matcher that admitted more than the walker
    /// would have the watcher mark files the indexer never stores, which is
    /// the orphaned-dirty-mark failure mode.
    inside_includes: Gitignore,
    /// `[ignore] <domain>`, the point-wise twin of the walker's check.
    domain_rules: DomainRules,
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
    pub fn build(root: &Path, domain: Domain) -> Self {
        let root = root.to_path_buf();
        let mut gi_builder = GitignoreBuilder::new(&root);

        let mut inside_builder = GitignoreBuilder::new(&root);

        let project_root = crate::project::resolve_project_root(&root);
        let includes = include_roots(&root, &project_root);
        let include_roots = includes.roots.clone();
        let tail_rules = includes.tail_rules.clone();
        let mut discovery = WalkBuilder::new(&root);
        discovery
            .hidden(false)
            .git_ignore(true)
            .require_git(false)
            .add_custom_ignore_filename(".infigraphignore");
        apply_filter_and_includes(
            &mut discovery,
            includes,
            DomainRules::for_walk(&root, &project_root, domain),
        );

        for result in discovery.build() {
            let Ok(entry) = result else { continue };
            let name = entry.file_name().to_string_lossy();
            if name != ".gitignore" && name != ".infigraphignore" {
                continue;
            }
            let _ = gi_builder.add(entry.path());
            if include_roots
                .iter()
                .any(|root| entry.path().starts_with(root))
            {
                let _ = inside_builder.add(entry.path());
            }
        }

        let gitignore = gi_builder.build().unwrap_or_else(|_| Gitignore::empty());
        let inside_includes = inside_builder
            .build()
            .unwrap_or_else(|_| Gitignore::empty());
        let domain_rules = DomainRules::for_walk(&root, &project_root, domain);
        IgnoreMatcher {
            root,
            gitignore,
            includes: include_roots,
            tail_rules,
            inside_includes,
            domain_rules,
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
        //
        // Without an include the check is below this matcher's root, never
        // the ancestors above it: a project checked out under `~/build/` or
        // `.../target/` is still a project, and the walker, which never
        // looks above its root, admits its files.
        let checked = match include {
            Some(root) => path.strip_prefix(root).unwrap_or(path),
            None => path.strip_prefix(&self.root).unwrap_or(path),
        };
        if checked
            .components()
            .any(|c| is_safety_excluded(&c.as_os_str().to_string_lossy()))
        {
            return true;
        }
        // The project's own "not worth indexing" rules survive an include,
        // matched against the tail exactly as the walker matches them.
        if include.is_some()
            && !checked.as_os_str().is_empty()
            && self.tail_rules.matched(checked, is_dir).is_ignore()
        {
            return true;
        }
        // A domain pattern is one more "not worth indexing" rule, and like
        // `.infigraphignore` it holds inside an include root.
        if self
            .domain_rules
            .ignores(path, is_dir, include.map(PathBuf::as_path))
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
        for result in walk_builder(dir.path(), Domain::Code).build() {
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
        let matcher = IgnoreMatcher::build(dir.path(), Domain::Code);

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

        let matcher = IgnoreMatcher::build(dir.path(), Domain::Code);
        assert!(matcher.is_ignored(&dir.path().join("vendored/lib.rs"), false));
        assert!(!matcher.is_ignored(&dir.path().join("real.rs"), false));

        let mut found = Vec::new();
        for result in walk_builder(dir.path(), Domain::Code).build() {
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

        let matcher = IgnoreMatcher::build(dir.path(), Domain::Code);
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
        walk_builder(root, Domain::Code)
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

    /// The safety list names directories *of the project*. A checkout that
    /// happens to live under `~/build/` or `.../target/` is still a project,
    /// and the watcher must not read every one of its files as excluded
    /// because an ancestor above the root has a listed name.
    #[test]
    fn the_matcher_judges_paths_below_its_root_not_the_ancestors_above_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("build").join("target").join("proj");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "//").unwrap();

        let walked = walked_files(&root);
        assert!(
            walked.iter().any(|p| p == "src/main.rs"),
            "the walker admits the project's own source: {walked:?}"
        );
        let matcher = IgnoreMatcher::build(&root, Domain::Code);
        assert!(
            !matcher.is_ignored(&root.join("src/main.rs"), false),
            "the matcher must agree: `build/` and `target/` above the root are not the project's"
        );
        assert!(
            matcher.is_ignored(&root.join("node_modules/pkg/index.js"), false),
            "a listed directory below the root is still excluded"
        );
    }

    #[test]
    fn the_ignore_matcher_agrees_with_the_walker_about_an_included_directory() {
        let dir = make_include_fixture();
        let matcher = IgnoreMatcher::build(dir.path(), Domain::Code);
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

        let matcher = IgnoreMatcher::build(dir.path(), Domain::Code);
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

        let matcher = IgnoreMatcher::build(dir.path(), Domain::Code);
        assert!(
            matcher.is_ignored(&lib.join("generated/bundle.js"), false),
            "the matcher must agree with the walker about rules from within"
        );
        assert!(!matcher.is_ignored(&lib.join("src/api.ts"), false));
    }

    /// pnpm's real store lives under `node_modules/.pnpm/`, and a package
    /// there is named by the exact path a project's own imports use. The
    /// walker runs with `hidden(true)`, so an include root under a
    /// dot-prefixed ancestor is worth pinning down: it works because the
    /// added root is where the walk *starts*, leaving `.pnpm` itself never
    /// visited as an entry -- but only until someone makes the filter look
    /// at ancestors.
    #[test]
    fn an_include_root_under_a_hidden_ancestor_is_walked() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
        fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
        fs::write(
            dir.path().join(".infigraph/config.toml"),
            "[index]\ninclude = [\"node_modules/.pnpm/pkg@1.0.0/node_modules/pkg\"]\n",
        )
        .unwrap();
        let pkg = dir
            .path()
            .join("node_modules/.pnpm/pkg@1.0.0/node_modules/pkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("grammar.js"), "module.exports = grammar({});").unwrap();
        fs::write(pkg.join(".hidden.js"), "// still hidden").unwrap();

        let found = walked_files(dir.path());
        assert!(
            found.iter().any(|p| p.ends_with("pkg/grammar.js")),
            "a `.pnpm` ancestor must not hide an include root: {found:?}"
        );
        assert!(
            !found.iter().any(|p| p.ends_with(".hidden.js")),
            "hidden(true) still applies to entries *within* an include root: {found:?}"
        );

        let matcher = IgnoreMatcher::build(dir.path(), Domain::Code);
        assert!(!matcher.is_ignored(&pkg.join("grammar.js"), false));
    }

    /// pnpm lays `node_modules/<pkg>` out as a symlink into `.pnpm/`, so
    /// the real-world include root is a link, not a directory. The walker
    /// runs with `follow_links(false)`, and whether that also refuses an
    /// added root is the kind of thing to check rather than assume.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_include_root_is_walked() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "node_modules/\n").unwrap();
        fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
        fs::write(
            dir.path().join(".infigraph/config.toml"),
            "[index]\ninclude = [\"node_modules/linked-lib\"]\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("node_modules/.store/linked-lib")).unwrap();
        fs::write(
            dir.path().join("node_modules/.store/linked-lib/grammar.js"),
            "module.exports = grammar({});",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("node_modules/.store/linked-lib"),
            dir.path().join("node_modules/linked-lib"),
        )
        .unwrap();

        let found = walked_files(dir.path());
        assert!(
            found.iter().any(|p| p.ends_with("grammar.js")),
            "a pnpm-style symlinked include root must still be walked: {found:?}"
        );

        let matcher = IgnoreMatcher::build(dir.path(), Domain::Code);
        assert!(
            !matcher.is_ignored(
                &dir.path().join("node_modules/linked-lib/grammar.js"),
                false
            ),
            "the matcher must agree about a symlinked include root"
        );
    }

    /// The seam between the two ignore files. `.gitignore` says "not under
    /// source control", which is exactly what an include overrides;
    /// `.infigraphignore` says "not worth indexing", which it has no
    /// business overriding. So the project's own `.infigraphignore` keeps
    /// applying inside an include root -- matched against the path
    /// *relative* to that root, so `node_modules/` stops matching (the
    /// include works) while `src/parser.c` still bites (2.7MB of generated
    /// parser tables stay out).
    #[test]
    fn the_project_infigraphignore_still_applies_inside_an_include_root() {
        let dir = make_include_fixture();
        fs::write(
            dir.path().join(".infigraphignore"),
            "node_modules/\nsrc/parser.c\n",
        )
        .unwrap();
        let lib = dir.path().join("node_modules/reference-lib");
        fs::write(lib.join("src/parser.c"), "// 2.7MB of tables, pretend").unwrap();

        let found = walked_files(dir.path());
        assert!(
            found
                .iter()
                .any(|p| p == "node_modules/reference-lib/src/api.ts"),
            "`node_modules/` must not match the tail, or the include is dead: {found:?}"
        );
        assert!(
            !found.iter().any(|p| p.ends_with("parser.c")),
            "a project .infigraphignore rule must reach inside an include root: {found:?}"
        );

        let matcher = IgnoreMatcher::build(dir.path(), Domain::Code);
        assert!(matcher.is_ignored(&lib.join("src/parser.c"), false));
        assert!(!matcher.is_ignored(&lib.join("src/api.ts"), false));
    }

    /// An entry may point outside the project only when it came from a
    /// layer the user controls. Their own `~/.infigraph/config.toml` may
    /// name a dependency's source in the cargo registry; a config file
    /// inside a repository they cloned may not name anything at all.
    #[test]
    fn an_entry_may_leave_the_project_only_from_a_trusted_layer() {
        let dir = make_include_fixture();
        let root = dir.path();
        let outside = root.join("node_modules");

        assert_eq!(
            classify_include(root, &outside.to_string_lossy(), Trust::User),
            Ok(outside.clone()),
            "the user's own config may name an absolute path"
        );
        assert_eq!(
            classify_include(root, &outside.to_string_lossy(), Trust::Project),
            Err(IncludeProblem::OutsideProject),
            "a project's config may not"
        );
        assert_eq!(
            classify_include(root, "../elsewhere", Trust::Project),
            Err(IncludeProblem::OutsideProject),
        );
        assert_eq!(
            classify_include(root, "node_modules/reference-lib", Trust::Project),
            Ok(root.join("node_modules/reference-lib")),
            "a relative in-project entry is fine from either layer"
        );
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
            classify_include(root, "node_modules/reference-lib", Trust::Project),
            Ok(root.join("node_modules/reference-lib")),
        );
        assert_eq!(
            classify_include(root, "node_modules/no-such-lib", Trust::Project),
            Err(IncludeProblem::NotADirectory),
        );
        assert_eq!(
            classify_include(
                root,
                "node_modules/reference-lib/src/api.ts",
                Trust::Project
            ),
            Err(IncludeProblem::NotADirectory),
            "include names directories; a file entry is a mistake, not a one-file include"
        );
        assert_eq!(
            classify_include(root, "../elsewhere", Trust::Project),
            Err(IncludeProblem::OutsideProject),
        );
        assert_eq!(
            classify_include(root, "/etc", Trust::Project),
            Err(IncludeProblem::OutsideProject),
        );
    }
}

#[cfg(test)]
mod shared_index_table_tests {
    use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};

    /// `[index]` holds this module's project-scoped `include` and the
    /// user-scoped reindex cap (`crate::slots`), declared as two settings
    /// groups. Each must resolve with the other's key in the same table.
    #[test]
    fn the_include_list_and_the_reindex_cap_share_the_index_table() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        std::env::remove_var(crate::slots::max_concurrent_reindexes_env_name());
        let dir = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".infigraph");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[index]\ninclude = [\"/somewhere/else\"]\nmax_concurrent_reindexes = 1\n",
        )
        .unwrap();

        assert_eq!(crate::slots::SlotPool::full_reindexes().unwrap().max(), 1);
        let project = tempfile::tempdir().unwrap();
        let includes = super::configured_includes(project.path());
        assert!(
            includes.iter().any(|(entry, _)| entry == "/somewhere/else"),
            "the include list stopped resolving beside the cap ({} entries)",
            includes.len()
        );
    }
}

/// `[ignore] code` / `[ignore] docs`: patterns that apply to one indexing
/// domain only.
#[cfg(test)]
mod domain_tests {
    use super::*;
    use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};
    use std::fs;

    const CODE_ENV: &str = "INFIGRAPH_IGNORE_CODE";
    const DOCS_ENV: &str = "INFIGRAPH_IGNORE_DOCS";

    /// Every file the fixtures below create, spelled relative to the
    /// project, so the matcher can be asked about each one.
    const FILES: &[&str] = &[
        "README.md",
        "docs/guide.md",
        "src/main.rs",
        "src/api.gen.rs",
        "src/docs/inner.md",
        "notes.draft.md",
        "user-only/a.rs",
        "proj-only/b.rs",
        "env-only/c.rs",
    ];

    fn project(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
        fs::write(dir.path().join(".infigraph/config.toml"), config).unwrap();
        for file in FILES {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
        }
        dir
    }

    fn walked(root: &Path, domain: Domain) -> Vec<String> {
        let mut found: Vec<String> = walk_builder(root, domain)
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
            .filter(|rel| FILES.contains(&rel.as_str()))
            .collect();
        found.sort();
        found
    }

    /// The watcher's matcher must answer exactly as the walker does for
    /// every file, in the same domain: a matcher more permissive than the
    /// walker has the watcher mark files the indexer never stores (#189).
    fn assert_matcher_agrees_with_walker(root: &Path, domain: Domain) {
        let walked = walked(root, domain);
        let matcher = IgnoreMatcher::build(root, domain);
        for file in FILES {
            let admitted_by_walker = walked.iter().any(|w| w == file);
            let admitted_by_matcher = !matcher.is_ignored(&root.join(file), false);
            assert_eq!(
                admitted_by_walker, admitted_by_matcher,
                "{domain:?}: walker admits {file}={admitted_by_walker} but matcher admits \
                 it={admitted_by_matcher}"
            );
        }
    }

    #[test]
    fn a_code_pattern_keeps_a_file_out_of_the_code_walk_but_in_the_docs_walk() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project("[ignore]\ncode = [\"docs/\", \"*.gen.rs\"]\n");

        let code = walked(dir.path(), Domain::Code);
        let docs = walked(dir.path(), Domain::Docs);

        assert!(!code.contains(&"docs/guide.md".into()), "{code:?}");
        assert!(!code.contains(&"src/docs/inner.md".into()), "{code:?}");
        assert!(!code.contains(&"src/api.gen.rs".into()), "{code:?}");
        assert!(code.contains(&"src/main.rs".into()), "{code:?}");
        for kept in ["docs/guide.md", "src/api.gen.rs", "src/docs/inner.md"] {
            assert!(
                docs.contains(&kept.to_string()),
                "a code-only pattern must not reach the docs walk: {kept} in {docs:?}"
            );
        }
    }

    #[test]
    fn a_docs_pattern_keeps_a_file_out_of_the_docs_walk_but_in_the_code_walk() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project("[ignore]\ndocs = [\"*.draft.md\"]\n");

        let docs = walked(dir.path(), Domain::Docs);
        let code = walked(dir.path(), Domain::Code);

        assert!(!docs.contains(&"notes.draft.md".into()), "{docs:?}");
        assert!(docs.contains(&"README.md".into()), "{docs:?}");
        assert!(
            code.contains(&"notes.draft.md".into()),
            "a docs-only pattern must not reach the code walk: {code:?}"
        );
    }

    #[test]
    fn the_matcher_agrees_with_the_walker_in_each_domain() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project(
            "[ignore]\ncode = [\"docs/\", \"*.gen.rs\", \"/proj-only/\"]\n\
             docs = [\"*.draft.md\", \"src/\"]\n",
        );
        assert_matcher_agrees_with_walker(dir.path(), Domain::Code);
        assert_matcher_agrees_with_walker(dir.path(), Domain::Docs);
        // And the fixture really does make the two domains differ, or the
        // agreement above proves nothing.
        assert_ne!(
            walked(dir.path(), Domain::Code),
            walked(dir.path(), Domain::Docs)
        );
    }

    #[test]
    fn project_and_user_layers_union_and_the_environment_replaces_both() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let user_dir = Path::new(&std::env::var("HOME").unwrap()).join(".infigraph");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(
            user_dir.join("config.toml"),
            "[ignore]\ncode = [\"user-only/\"]\n",
        )
        .unwrap();
        let dir = project("[ignore]\ncode = [\"proj-only/\"]\n");
        std::env::remove_var(CODE_ENV);

        let both = walked(dir.path(), Domain::Code);
        assert!(!both.contains(&"user-only/a.rs".into()), "{both:?}");
        assert!(!both.contains(&"proj-only/b.rs".into()), "{both:?}");
        assert!(both.contains(&"env-only/c.rs".into()), "{both:?}");

        std::env::set_var(CODE_ENV, "env-only/");
        let from_env = walked(dir.path(), Domain::Code);
        std::env::remove_var(CODE_ENV);
        assert!(!from_env.contains(&"env-only/c.rs".into()), "{from_env:?}");
        assert!(
            from_env.contains(&"user-only/a.rs".into())
                && from_env.contains(&"proj-only/b.rs".into()),
            "the environment replaces the files, so their lists no longer apply: {from_env:?}"
        );
    }

    #[test]
    fn the_docs_environment_variable_is_its_own_domain() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project("");
        std::env::set_var(DOCS_ENV, "env-only/, notes.draft.md");
        let docs = walked(dir.path(), Domain::Docs);
        let code = walked(dir.path(), Domain::Code);
        std::env::remove_var(DOCS_ENV);

        assert!(!docs.contains(&"env-only/c.rs".into()), "{docs:?}");
        assert!(!docs.contains(&"notes.draft.md".into()), "{docs:?}");
        assert!(code.contains(&"env-only/c.rs".into()), "{code:?}");
    }

    #[test]
    fn a_parent_component_is_dropped_and_a_leading_slash_anchors_at_the_project() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        // `../src/` could only ever mean "outside the project", where
        // nothing is walked, so it must be inert -- never read as `src/`.
        let dir = project("[ignore]\ncode = [\"../src/\", \"/docs/\"]\n");

        let code = walked(dir.path(), Domain::Code);
        assert!(code.contains(&"src/main.rs".into()), "{code:?}");
        assert!(!code.contains(&"docs/guide.md".into()), "{code:?}");
        assert!(
            code.contains(&"src/docs/inner.md".into()),
            "`/docs/` anchors at the project root, so it must not match src/docs: {code:?}"
        );
        assert_matcher_agrees_with_walker(dir.path(), Domain::Code);
    }

    #[test]
    fn an_unanchored_directory_pattern_matches_at_any_depth() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project("[ignore]\ncode = [\"docs/\"]\n");
        let code = walked(dir.path(), Domain::Code);
        assert!(!code.contains(&"docs/guide.md".into()), "{code:?}");
        assert!(!code.contains(&"src/docs/inner.md".into()), "{code:?}");
    }

    #[test]
    fn a_walk_of_a_subdirectory_still_anchors_at_the_project_root() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project("[ignore]\ncode = [\"/src/docs/\"]\n");
        // A directory is a project store only once it holds a graph.
        fs::write(dir.path().join(".infigraph/graph"), b"x").unwrap();
        let src = dir.path().join("src");

        let found: Vec<String> = walk_builder(&src, Domain::Code)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
            .map(|entry| {
                entry
                    .path()
                    .strip_prefix(&src)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(found.contains(&"main.rs".to_string()), "{found:?}");
        assert!(
            !found.contains(&"docs/inner.md".to_string()),
            "a walk rooted at src/ must still read `/src/docs/` against the project: {found:?}"
        );
    }

    /// A re-admitted library is still full of generated files the project
    /// did not want: the pattern holds below an include root, as
    /// `.infigraphignore` does, and the watcher agrees.
    #[test]
    fn a_domain_pattern_still_applies_inside_an_include_root() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        fs::create_dir_all(root.join(".infigraph")).unwrap();
        fs::write(
            root.join(".infigraph/config.toml"),
            "[index]\ninclude = [\"node_modules/lib\"]\n[ignore]\ncode = [\"*.gen.ts\"]\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("node_modules/lib/src")).unwrap();
        fs::write(root.join("node_modules/lib/src/api.ts"), "x").unwrap();
        fs::write(root.join("node_modules/lib/src/api.gen.ts"), "x").unwrap();

        let rel = |domain: Domain| -> Vec<String> {
            walk_builder(root, domain)
                .build()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()))
                .filter_map(|e| {
                    let r = e.path().strip_prefix(root).ok()?.to_path_buf();
                    Some(r.to_string_lossy().replace('\\', "/"))
                })
                .filter(|p| p.starts_with("node_modules/lib"))
                .collect()
        };
        let code = rel(Domain::Code);
        assert!(code.contains(&"node_modules/lib/src/api.ts".to_string()));
        assert!(!code.contains(&"node_modules/lib/src/api.gen.ts".to_string()));
        assert!(rel(Domain::Docs).contains(&"node_modules/lib/src/api.gen.ts".to_string()));

        let matcher = IgnoreMatcher::build(root, Domain::Code);
        assert!(matcher.is_ignored(&root.join("node_modules/lib/src/api.gen.ts"), false));
        assert!(!matcher.is_ignored(&root.join("node_modules/lib/src/api.ts"), false));
    }

    #[test]
    fn the_group_is_registered_and_a_lone_string_is_a_reported_error() {
        assert!(crate::settings::registered_categories().contains(&"ignore"));

        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        let dir = project("[ignore]\ncode = \"docs/\"\n");
        let problems = crate::settings::check_all(ConfigScope::Project(dir.path()));
        assert!(
            problems.iter().any(|p| p.setting.contains("[ignore] code")),
            "a bare string is the likely mistake and must not read as no patterns: {problems:?}"
        );
    }
}
