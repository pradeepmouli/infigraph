//! Semantic diff between two git refs at the symbol level.
//!
//! Instead of a line diff, this compares the extracted symbol graphs of two
//! git tree-states and classifies each change as Added / Removed / Modified /
//! SignatureChanged.  The caller supplies a project root and two git refs
//! (e.g. "HEAD~1", "main"); the module checks out each ref into a temp
//! worktree, indexes it with the current language registry, and returns a
//! structured `SymbolDiff`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::extract;
use crate::lang::LanguageRegistry;

/// How a symbol changed between two refs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChangeKind {
    /// Symbol exists in new ref but not in old ref.
    Added,
    /// Symbol exists in old ref but not in new ref.
    Removed,
    /// Symbol exists in both; signature_hash changed (parameter / return type change).
    SignatureChanged,
    /// Symbol exists in both; body changed but signature is the same.
    Modified,
    /// Symbol moved to a different file.
    Moved { from_file: String },
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeKind::Added => write!(f, "ADDED"),
            ChangeKind::Removed => write!(f, "REMOVED"),
            ChangeKind::SignatureChanged => write!(f, "SIGNATURE_CHANGED"),
            ChangeKind::Modified => write!(f, "MODIFIED"),
            ChangeKind::Moved { from_file } => write!(f, "MOVED(from:{})", from_file),
        }
    }
}

/// A single symbol-level change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolChange {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub change: ChangeKind,
    /// Callers in the current graph (populated by caller when graph is available).
    pub caller_count: usize,
}

/// Full semantic diff result.
#[derive(Debug, Default)]
pub struct SymbolDiff {
    pub old_ref: String,
    pub new_ref: String,
    pub changes: Vec<SymbolChange>,
}

impl SymbolDiff {
    pub fn added(&self) -> impl Iterator<Item = &SymbolChange> {
        self.changes.iter().filter(|c| c.change == ChangeKind::Added)
    }
    pub fn removed(&self) -> impl Iterator<Item = &SymbolChange> {
        self.changes.iter().filter(|c| c.change == ChangeKind::Removed)
    }
    pub fn modified(&self) -> impl Iterator<Item = &SymbolChange> {
        self.changes.iter().filter(|c| {
            matches!(c.change, ChangeKind::Modified | ChangeKind::SignatureChanged | ChangeKind::Moved { .. })
        })
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A flat symbol record used during diff (file + name + kind + sig_hash).
#[derive(Clone)]
struct FlatSym {
    file: String,
    name: String,
    kind: String,
    sig_hash: String,
}

/// Compute a symbol-level diff between `old_ref` and `new_ref` in `project_root`.
///
/// Uses `git archive` to extract each ref into a temp directory so no
/// working-tree modifications are needed.
pub fn semantic_diff(
    project_root: &Path,
    old_ref: &str,
    new_ref: &str,
    registry: &LanguageRegistry,
) -> Result<SymbolDiff> {
    let old_symbols = extract_ref_symbols(project_root, old_ref, registry)
        .with_context(|| format!("failed to extract symbols for ref '{}'", old_ref))?;
    let new_symbols = extract_ref_symbols(project_root, new_ref, registry)
        .with_context(|| format!("failed to extract symbols for ref '{}'", new_ref))?;

    Ok(diff_symbol_maps(old_ref, new_ref, old_symbols, new_symbols))
}

// ---------------------------------------------------------------------------
// Extract symbols for a git ref
// ---------------------------------------------------------------------------

/// Extract all symbols from a git ref by using `git archive | tar -x` into a
/// temp directory, then walking files through the language registry.
fn extract_ref_symbols(
    project_root: &Path,
    git_ref: &str,
    registry: &LanguageRegistry,
) -> Result<HashMap<String, FlatSym>> {
    // For HEAD/working-tree we skip the archive step and read directly.
    let is_working_tree = git_ref == "HEAD" || git_ref == "WORKING";

    if is_working_tree {
        return extract_dir_symbols(project_root, project_root, registry);
    }

    // Use git archive to extract the ref
    let tmp = tempfile::tempdir().context("failed to create temp dir")?;
    let archive_output = std::process::Command::new("git")
        .args(["archive", "--format=tar", git_ref])
        .current_dir(project_root)
        .output()
        .context("git archive failed")?;

    if !archive_output.status.success() {
        let err = String::from_utf8_lossy(&archive_output.stderr);
        anyhow::bail!("git archive {} failed: {}", git_ref, err.trim());
    }

    // Extract tar to tmp
    let mut tar = std::process::Command::new("tar")
        .args(["-x", "-C", tmp.path().to_str().unwrap_or(".")])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn tar")?;

    if let Some(stdin) = tar.stdin.take() {
        use std::io::Write;
        let mut w = stdin;
        w.write_all(&archive_output.stdout)?;
    }
    tar.wait().context("tar wait failed")?;

    extract_dir_symbols(tmp.path(), tmp.path(), registry)
}

fn extract_dir_symbols(
    root: &Path,
    dir: &Path,
    registry: &LanguageRegistry,
) -> Result<HashMap<String, FlatSym>> {
    let mut map = HashMap::new();
    collect_symbols(root, dir, registry, &mut map)?;
    Ok(map)
}

static SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", ".venv", "venv", "target", "build",
    "dist", "__pycache__", ".tox", ".infigraph",
];

fn collect_symbols(
    root: &Path,
    dir: &Path,
    registry: &LanguageRegistry,
    map: &mut HashMap<String, FlatSym>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if path.is_dir() {
            if !SKIP_DIRS.contains(&name_str.as_ref()) && !name_str.starts_with('.') {
                collect_symbols(root, &path, registry, map)?;
            }
        } else if path.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            let Ok(source) = std::fs::read(&path) else { continue };
            let Some(pack) = registry.for_file_with_content(&rel, &source) else { continue };
            let Ok(extraction) = extract::extract_file(&rel, &source, pack) else { continue };
            let file = extraction.file.clone();
            for sym in &extraction.symbols {
                let kind_str = sym.kind.as_str().to_string();
                // Key: "file::name::kind" — stable across refs
                let key = format!("{}::{}::{}", file, sym.name, kind_str);
                map.insert(key, FlatSym {
                    file: file.clone(),
                    name: sym.name.clone(),
                    kind: kind_str,
                    sig_hash: sym.signature_hash.clone(),
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Diff two symbol maps
// ---------------------------------------------------------------------------

fn diff_symbol_maps(
    old_ref: &str,
    new_ref: &str,
    old: HashMap<String, FlatSym>,
    new: HashMap<String, FlatSym>,
) -> SymbolDiff {
    let mut changes = Vec::new();

    // Build name→sym map for old (for move detection)
    let old_by_name: HashMap<String, &FlatSym> = old
        .values()
        .map(|s| (format!("{}::{}", s.name, s.kind), s))
        .collect();

    // Check new symbols against old
    for (key, new_sym) in &new {
        if let Some(old_sym) = old.get(key) {
            // Same file+name+kind — check signature change
            if old_sym.sig_hash != new_sym.sig_hash
                && !old_sym.sig_hash.is_empty()
                && !new_sym.sig_hash.is_empty()
            {
                changes.push(SymbolChange {
                    name: new_sym.name.clone(),
                    kind: new_sym.kind.clone(),
                    file: new_sym.file.clone(),
                    change: ChangeKind::SignatureChanged,
                    caller_count: 0,
                });
            }
        } else {
            // Not in old by key. Check if name+kind existed in a different file (move).
            let name_key = format!("{}::{}", new_sym.name, new_sym.kind);
            if let Some(old_sym) = old_by_name.get(&name_key) {
                if old_sym.file != new_sym.file {
                    changes.push(SymbolChange {
                        name: new_sym.name.clone(),
                        kind: new_sym.kind.clone(),
                        file: new_sym.file.clone(),
                        change: ChangeKind::Moved { from_file: old_sym.file.clone() },
                        caller_count: 0,
                    });
                    continue;
                }
            }
            // Truly new
            changes.push(SymbolChange {
                name: new_sym.name.clone(),
                kind: new_sym.kind.clone(),
                file: new_sym.file.clone(),
                change: ChangeKind::Added,
                caller_count: 0,
            });
        }
    }

    // Removed: in old but not in new (excluding moves already captured)
    let moved_names: std::collections::HashSet<String> = changes
        .iter()
        .filter_map(|c| if matches!(c.change, ChangeKind::Moved { .. }) {
            Some(format!("{}::{}", c.name, c.kind))
        } else {
            None
        })
        .collect();

    for (key, old_sym) in &old {
        if !new.contains_key(key) {
            let name_key = format!("{}::{}", old_sym.name, old_sym.kind);
            if !moved_names.contains(&name_key) {
                changes.push(SymbolChange {
                    name: old_sym.name.clone(),
                    kind: old_sym.kind.clone(),
                    file: old_sym.file.clone(),
                    change: ChangeKind::Removed,
                    caller_count: 0,
                });
            }
        }
    }

    // Sort: Removed first, then Added, then modified kinds
    changes.sort_by_key(|c| match &c.change {
        ChangeKind::Removed => 0,
        ChangeKind::SignatureChanged => 1,
        ChangeKind::Modified => 2,
        ChangeKind::Moved { .. } => 3,
        ChangeKind::Added => 4,
    });

    SymbolDiff {
        old_ref: old_ref.to_string(),
        new_ref: new_ref.to_string(),
        changes,
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

pub fn format_diff(diff: &SymbolDiff) -> String {
    if diff.changes.is_empty() {
        return format!(
            "No symbol-level changes between '{}' and '{}'.",
            diff.old_ref, diff.new_ref
        );
    }

    let added = diff.added().count();
    let removed = diff.removed().count();
    let modified = diff.modified().count();

    let mut out = format!(
        "Semantic diff {} → {}  [+{} added  -{} removed  ~{} modified]\n\n",
        diff.old_ref, diff.new_ref, added, removed, modified
    );

    let mut cur_file = String::new();
    for c in &diff.changes {
        if c.file != cur_file {
            out.push_str(&format!("  {}\n", c.file));
            cur_file = c.file.clone();
        }
        let callers = if c.caller_count > 0 {
            format!("  [{} callers]", c.caller_count)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "    {:>20}  {:<10} {}{}\n",
            c.change.to_string(), c.kind, c.name, callers
        ));
    }

    out
}
